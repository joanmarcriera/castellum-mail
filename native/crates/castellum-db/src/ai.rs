// Castellum - AI job queue repository layer.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Typed CRUD/query methods for the AI job queue (tables: ai_jobs, ai_job_events,
// ai_outputs). All methods are on `Database` and follow the same conventions as
// repo.rs: single serialized connection, transactions for multi-row writes, no
// raw SQL outside this file.
//
// Privacy contract (enforced here):
//   * payload_json fields store IDs (refs) only — never message content.
//     `enqueue_ai_job` rejects any payload key outside
//     `AI_JOB_PAYLOAD_ALLOWED_KEYS`, so a new AI feature cannot quietly
//     persist subject/recipients/body (the S16 first-attempt mistake).
//   * error_message fields carry typed variant info only (model name / hostname).
//   * No prompt text ever enters these rows.

use crate::{Database, DbError};
use rusqlite::{params, params_from_iter, OptionalExtension};

// ---------------------------------------------------------------------------
// Public record types
// ---------------------------------------------------------------------------

/// A row from `ai_jobs`, returned by next_due / poll / list queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiJobRecord {
    pub id: i64,
    pub account_id: Option<String>,
    pub feature: String,
    pub payload_json: String,
    pub priority: i64,
    pub status: String,
    pub cancel_requested: bool,
    pub model: Option<String>,
    pub attempts: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// `updated_at` as Unix epoch seconds (for backoff calculation).
    pub updated_at_epoch: i64,
}

/// A row from `ai_job_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiEventRecord {
    pub id: i64,
    pub job_id: i64,
    pub kind: String,
    pub payload_json: String,
}

/// A row from `message_embeddings` (S52). `vector` is the raw little-endian
/// f32 bytes — never a `Vec<f32>` here, so nothing in this crate is tempted
/// to `{:?}`-format the floats into a log or error message. Callers that need
/// the floats back (S53's similarity search) decode them explicitly at the
/// point of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEmbeddingRecord {
    pub message_id: String,
    pub model_name: String,
    pub dimension: i64,
    pub vector: Vec<u8>,
}

/// A row from `ai_outputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiOutputRecord {
    pub id: i64,
    pub job_id: i64,
    pub scope: String,
    pub owner_id: String,
    pub feature: String,
    pub text: String,
    pub complete: bool,
    pub model: Option<String>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// The only keys `ai_jobs.payload_json` may hold. Refs (`message_id`,
/// `thread_id`) plus the user's short `instruction`, which the `draft_reply`
/// feature has always stored so a retried job can rebuild its prompt.
/// Adding a key here is a privacy decision: record it in DECISIONS.md.
pub const AI_JOB_PAYLOAD_ALLOWED_KEYS: &[&str] = &["message_id", "thread_id", "instruction"];

/// `ai_jobs.priority` scale (ASC = highest priority first): 0 interactive,
/// 1 prefetch, 2 background, 3 maintenance. `embed_message` jobs (S52) are
/// enqueued at background priority from the sync/hydration ingest paths
/// (`castellum_core::sync::enqueue_embed_job`) so indexing never competes
/// with, or delays, interactive/prefetch AI work — or sync itself, which
/// never waits on this queue at all (enqueuing is a single `INSERT`).
pub const AI_PRIORITY_BACKGROUND: i64 = 2;

/// Per-call chunk size for [`Database::get_message_embeddings`]' bulk `IN
/// (...)` lookup (S53-UI). One bound SQL variable per id; 500 is
/// comfortably under any SQLite build's ~32766 variable ceiling while still
/// batching generously, so a caller passing thousands of ids never emits a
/// query with more bound variables than SQLite allows.
pub const EMBEDDINGS_QUERY_CHUNK: usize = 500;

/// Reject a payload that is not a JSON object, or that carries a key outside
/// the allowlist, or a non-string value (refs and instructions are strings;
/// anything nested could smuggle content past the key check).
fn check_ai_job_payload(payload_json: &str) -> Result<(), DbError> {
    let value: serde_json::Value = serde_json::from_str(payload_json)
        .map_err(|e| DbError(format!("ai job payload is not JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| DbError("ai job payload must be a JSON object".into()))?;
    for (key, val) in obj {
        if !AI_JOB_PAYLOAD_ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(DbError(format!(
                "ai job payload key '{key}' is not allowed (refs only, never content)"
            )));
        }
        if !val.is_string() {
            return Err(DbError(format!(
                "ai job payload key '{key}' must be a string"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// impl Database — AI job queue
// ---------------------------------------------------------------------------

impl Database {
    // -----------------------------------------------------------------------
    // Enqueue
    // -----------------------------------------------------------------------

    /// Insert a new job in `queued` status and return its id.
    ///
    /// Fails without writing if `payload_json` breaks the privacy contract
    /// (see `AI_JOB_PAYLOAD_ALLOWED_KEYS`).
    pub fn enqueue_ai_job(
        &self,
        account_id: Option<&str>,
        feature: &str,
        payload_json: &str,
        priority: i64,
    ) -> Result<i64, DbError> {
        check_ai_job_payload(payload_json)?;
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO ai_jobs (account_id, feature, payload_json, priority)
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, feature, payload_json, priority],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // -----------------------------------------------------------------------
    // Poll for the next due job
    // -----------------------------------------------------------------------

    /// Return the next job that should be attempted now.
    ///
    /// A job is "due" if:
    ///   - status = 'queued', or
    ///   - status = 'failed' AND attempts < 3 AND `updated_at_epoch + backoff <= now_epoch`
    ///
    /// Ordered: priority ASC (0 = highest), then id ASC (FIFO within same priority).
    pub fn next_due_ai_job(&self, now_epoch: i64) -> Result<Option<AiJobRecord>, DbError> {
        self.next_due_ai_job_scoped(now_epoch, None, None)
    }

    /// [`Self::next_due_ai_job`] restricted by feature: `only_feature` keeps
    /// just that feature, `exclude_feature` drops it. Both `None` is the
    /// unscoped query. Lets the embeddings drainer (and the interactive chat
    /// runner) each see only their own half of the one shared queue.
    pub fn next_due_ai_job_scoped(
        &self,
        now_epoch: i64,
        only_feature: Option<&str>,
        exclude_feature: Option<&str>,
    ) -> Result<Option<AiJobRecord>, DbError> {
        self.next_due_ai_job_excluding(now_epoch, only_feature, exclude_feature, &[])
    }

    /// [`Self::next_due_ai_job_scoped`] that also skips the given job ids, so
    /// an executor pass can move on to the next candidate instead of
    /// re-selecting a job it already attempted (a failed job with a zero
    /// backoff is immediately due again and would otherwise shadow every job
    /// behind it).
    pub fn next_due_ai_job_excluding(
        &self,
        now_epoch: i64,
        only_feature: Option<&str>,
        exclude_feature: Option<&str>,
        skip_ids: &[i64],
    ) -> Result<Option<AiJobRecord>, DbError> {
        let conn = self.lock_conn();
        // Integers only, so inlining them cannot inject anything.
        let skip_sql = if skip_ids.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = skip_ids.iter().map(|i| i.to_string()).collect();
            format!(" AND id NOT IN ({})", list.join(","))
        };
        let sql = format!(
            "SELECT id, account_id, feature, payload_json, priority, status,
                        cancel_requested, model, attempts, error_code, error_message,
                        CAST(strftime('%s', updated_at) AS INTEGER)
                 FROM ai_jobs
                 WHERE ((status = 'queued')
                    OR (status = 'failed'
                        AND attempts < 3
                        AND CAST(strftime('%s', updated_at) AS INTEGER)
                            + (attempts - 1) * (attempts - 1) <= ?1))
                   AND (?2 IS NULL OR feature = ?2)
                   AND (?3 IS NULL OR feature <> ?3){skip_sql}
                 ORDER BY priority ASC, id ASC
                 LIMIT 1"
        );
        let row = conn
            .query_row(
                &sql,
                params![now_epoch, only_feature, exclude_feature],
                row_to_ai_job,
            )
            .optional()?;
        Ok(row)
    }

    /// Put a just-leased job back to `queued` and refund the attempt the lease
    /// charged. For failures that say nothing about the job itself (endpoint
    /// down, embedding model unusable): the backlog must survive an outage
    /// instead of burning each job's three attempts.
    pub fn requeue_ai_job_refund_attempt(&self, id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE ai_jobs
             SET status = 'queued', attempts = MAX(attempts - 1, 0),
                 updated_at = datetime('now')
             WHERE id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![id],
        )?;
        Ok(())
    }

    /// Park an embed job the model rejects (HTTP 500 even at the shortest
    /// text) as terminally failed, recording which model rejected it so
    /// [`Self::requeue_rejected_embed_jobs`] can retry under a different one.
    /// `error_code`/`error_message` are fixed strings, never backend text.
    pub fn park_rejected_embed_job(
        &self,
        id: i64,
        max_attempts_ai: i64,
        model: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE ai_jobs
             SET status = 'failed', error_code = 'embed_rejected',
                 error_message = 'model rejected this message', model = ?3,
                 attempts = ?2, finished_at = datetime('now'),
                 updated_at = datetime('now')
             WHERE id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![id, max_attempts_ai, model],
        )?;
        Ok(())
    }

    /// Give parked (`embed_rejected`) embed jobs a fresh start when the
    /// embedding model differs from the one that rejected them. Returns the
    /// number requeued.
    pub fn requeue_rejected_embed_jobs(&self, model: &str) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE ai_jobs
             SET status = 'queued', attempts = 0, error_code = NULL,
                 error_message = NULL, finished_at = NULL, model = NULL,
                 updated_at = datetime('now')
             WHERE feature = 'embed_message' AND status = 'failed'
               AND error_code = 'embed_rejected'
               AND COALESCE(model, '') <> ?1",
            params![model],
        )?)
    }

    /// Backfill: enqueue `embed_message` jobs for messages the ingest hook never
    /// covered, so Find Similar can cover the whole mailbox and not only mail
    /// that arrives after the embedding model was chosen. Two kinds of message
    /// qualify (newest first, at most `limit`):
    ///   * a body but no vector and no embed job of any status (mail ingested
    ///     before the job existed, or whose job history was pruned);
    ///   * a vector made by a different model than `model` and no live job
    ///     (queued/in flight/failed) -- a model change must re-index, because
    ///     vectors of different models are not comparable.
    ///
    /// Returns the number enqueued. The per-message "has a job?" probes use the
    /// partial expression index `idx_ai_jobs_embed_msg` (migration 0052), so a
    /// sweep is one pass over `messages` plus index seeks, not a scan of
    /// `ai_jobs` per message. The `+m.id` keeps the index usable (TEXT affinity).
    pub fn enqueue_missing_embed_jobs(&self, model: &str, limit: i64) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            Self::ENQUEUE_MISSING_EMBED_SQL,
            params![model, limit, AI_PRIORITY_BACKGROUND],
        )?)
    }

    /// Statement behind [`Self::enqueue_missing_embed_jobs`]; a const so a test can
    /// run `EXPLAIN QUERY PLAN` on exactly what production runs.
    const ENQUEUE_MISSING_EMBED_SQL: &'static str =
            "INSERT INTO ai_jobs (account_id, feature, payload_json, priority)
             SELECT m.account_id, 'embed_message',
                    json_object('message_id', m.id), ?3
             FROM messages m
             JOIN message_bodies b ON b.message_id = m.id
             LEFT JOIN message_embeddings e ON e.message_id = m.id
             WHERE ((e.message_id IS NULL
                     AND NOT EXISTS (SELECT 1 FROM ai_jobs j
                                     WHERE j.feature = 'embed_message'
                                       AND json_extract(j.payload_json, '$.message_id') = +m.id))
                 OR (e.message_id IS NOT NULL AND e.model_name <> ?1
                     AND NOT EXISTS (SELECT 1 FROM ai_jobs j
                                     WHERE j.feature = 'embed_message'
                                       AND j.status IN ('queued','leased','running','streaming','failed')
                                       AND json_extract(j.payload_json, '$.message_id') = +m.id)))
             ORDER BY m.sent_at DESC, m.id
             LIMIT ?2";

    /// Embedding jobs still to do: queued, in flight, or failed with attempts
    /// left. The "queued" figure the AI Activity window shows.
    pub fn count_pending_embed_jobs(&self) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM ai_jobs
             WHERE feature = 'embed_message'
               AND (status IN ('queued', 'leased', 'running')
                    OR (status = 'failed' AND attempts < 3))",
            [],
            |r| r.get(0),
        )?)
    }

    /// Stored message vectors: the "done" figure.
    pub fn count_message_embeddings(&self) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        Ok(conn.query_row("SELECT COUNT(*) FROM message_embeddings", [], |r| r.get(0))?)
    }

    /// Whether any vector was ever stored by `model_name`: proof that the model
    /// can embed, so a later run of rejected messages is bad messages, not a bad
    /// model (audit #3440).
    pub fn has_embedding_for_model(&self, model_name: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_embeddings WHERE model_name = ?1)",
            params![model_name],
            |r| r.get(0),
        )?)
    }

    // -----------------------------------------------------------------------
    // Stale recovery
    // -----------------------------------------------------------------------

    /// At executor startup: jobs left in `leased`/`running`/`streaming` from a
    /// prior crash are re-queued if `attempts < 3`; otherwise they are marked
    /// `failed` with `error_code = 'backend'`.
    ///
    /// Returns the number of jobs touched.
    pub fn recover_stale_ai_jobs(&self) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        // Find stale ids and their attempt counts.
        let stale: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id, attempts FROM ai_jobs
                 WHERE status IN ('leased', 'running', 'streaming')",
            )?;
            let rows: Vec<(i64, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            rows
        };

        let count = stale.len();
        for (id, attempts) in stale {
            let cancelled: bool = tx.query_row(
                "SELECT cancel_requested != 0 FROM ai_jobs WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            if cancelled {
                // The user cancelled before the crash: do not run it again.
                tx.execute(
                    "UPDATE ai_jobs
                     SET status = 'cancelled', finished_at = datetime('now'),
                         updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "INSERT INTO ai_job_events (job_id, kind, payload_json)
                     VALUES (?1, 'cancelled', '{}')",
                    params![id],
                )?;
            } else if attempts < 3 {
                tx.execute(
                    "UPDATE ai_jobs
                     SET status = 'queued', updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
            } else {
                tx.execute(
                    "UPDATE ai_jobs
                     SET status = 'failed', error_code = 'backend',
                         error_message = 'recovered from stale state after crash',
                         finished_at = datetime('now'), updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "INSERT INTO ai_job_events (job_id, kind, payload_json)
                     VALUES (?1, 'failed', '{\"reason\":\"recovered_stale\"}')",
                    params![id],
                )?;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // State machine transitions
    // -----------------------------------------------------------------------

    /// Transition a job to `leased` and increment `attempts`.
    ///
    /// Returns the post-lease `attempts` value, read back from the row inside
    /// the same lock — callers must not compute it from a stale snapshot.
    /// Returns `DbError` if the id is unknown.
    pub fn lease_ai_job(&self, id: i64) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE ai_jobs
             SET status = 'leased', attempts = attempts + 1,
                 started_at = COALESCE(started_at, datetime('now')),
                 updated_at = datetime('now')
             WHERE id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![id],
        )?;
        if changed == 0 {
            guard_exists(&conn, id)?;
        }
        let attempts: i64 = conn.query_row(
            "SELECT attempts FROM ai_jobs WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(attempts)
    }

    /// Update a job's status, error_code, and error_message.
    ///
    /// Terminal statuses (`succeeded`, `cancelled`) are immutable: a race
    /// between the executor and a cancel request must not corrupt the record.
    /// `failed` remains retryable (executor controls whether it ever gets
    /// dequeued again via the attempts cap in `next_due_ai_job`).
    pub fn set_ai_job_status(
        &self,
        id: i64,
        status: &str,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let finish = matches!(status, "succeeded" | "cancelled" | "failed");
        let changed = conn.execute(
            "UPDATE ai_jobs
             SET status = ?2, error_code = ?3, error_message = ?4,
                 finished_at = CASE WHEN ?5 THEN datetime('now') ELSE finished_at END,
                 updated_at = datetime('now')
             WHERE id = ?1
               AND status NOT IN ('succeeded', 'cancelled')",
            params![id, status, error_code, error_message, finish],
        )?;
        if changed == 0 {
            guard_exists(&conn, id)?;
            // Already terminal — benign race, leave it be.
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Cancellation
    // -----------------------------------------------------------------------

    /// Cancel a job:
    ///   - `queued` → immediately set `cancelled` + append `cancelled` event.
    ///   - in-flight (leased/running/streaming) → set `cancel_requested = 1`;
    ///     the executor checks this flag and cooperatively stops.
    ///   - already terminal → no-op.
    pub fn request_ai_cancel(&self, id: i64) -> Result<(), DbError> {
        let mut conn = self.lock_conn();

        // Read current status.
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM ai_jobs WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;

        let Some(status) = status else {
            return Err(DbError(format!("unknown ai_job id: {id}")));
        };

        // `failed` with attempts left is still runnable (see
        // `next_due_ai_job_scoped`), so it is cancellable like `queued`.
        let retryable_failed = status == "failed"
            && conn.query_row(
                "SELECT attempts < 3 FROM ai_jobs WHERE id = ?1",
                params![id],
                |r| r.get::<_, bool>(0),
            )?;
        match status.as_str() {
            "queued" => {
                let tx = conn.transaction()?;
                tx.execute(
                    "UPDATE ai_jobs
                     SET status = 'cancelled', cancel_requested = 1,
                         finished_at = datetime('now'), updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "INSERT INTO ai_job_events (job_id, kind, payload_json)
                     VALUES (?1, 'cancelled', '{}')",
                    params![id],
                )?;
                tx.commit()?;
            }
            "failed" if retryable_failed => {
                let tx = conn.transaction()?;
                tx.execute(
                    "UPDATE ai_jobs
                     SET status = 'cancelled', cancel_requested = 1,
                         finished_at = datetime('now'), updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "INSERT INTO ai_job_events (job_id, kind, payload_json)
                     VALUES (?1, 'cancelled', '{}')",
                    params![id],
                )?;
                tx.commit()?;
            }
            "leased" | "running" | "streaming" => {
                conn.execute(
                    "UPDATE ai_jobs SET cancel_requested = 1, updated_at = datetime('now')
                     WHERE id = ?1",
                    params![id],
                )?;
            }
            // Already terminal — no-op.
            _ => {}
        }
        Ok(())
    }

    /// Check whether a cancel has been requested for an in-flight job.
    /// The executor calls this inside the delta callback.
    pub fn is_ai_cancel_requested(&self, id: i64) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let v: Option<i64> = conn
            .query_row(
                "SELECT cancel_requested FROM ai_jobs WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0) != 0)
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    /// Append a streaming event and return its id.
    pub fn append_ai_event(
        &self,
        job_id: i64,
        kind: &str,
        payload_json: &str,
    ) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO ai_job_events (job_id, kind, payload_json) VALUES (?1, ?2, ?3)",
            params![job_id, kind, payload_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return the current job record plus all events with `id > after_event_id`.
    /// Returns `None` if the job id is unknown.
    pub fn poll_ai_job(
        &self,
        job_id: i64,
        after_event_id: i64,
    ) -> Result<Option<(AiJobRecord, Vec<AiEventRecord>)>, DbError> {
        let conn = self.lock_conn();

        let job = conn
            .query_row(
                "SELECT id, account_id, feature, payload_json, priority, status,
                        cancel_requested, model, attempts, error_code, error_message,
                        CAST(strftime('%s', updated_at) AS INTEGER)
                 FROM ai_jobs WHERE id = ?1",
                params![job_id],
                row_to_ai_job,
            )
            .optional()?;

        let Some(job) = job else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT id, job_id, kind, payload_json FROM ai_job_events
             WHERE job_id = ?1 AND id > ?2
             ORDER BY id ASC",
        )?;
        let events: Vec<AiEventRecord> = stmt
            .query_map(params![job_id, after_event_id], |r| {
                Ok(AiEventRecord {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    kind: r.get(2)?,
                    payload_json: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;

        Ok(Some((job, events)))
    }

    /// Payload JSON of the job's most recent `completed` event, if any. The
    /// executor stores `duration_ms` / `tokens_*` there (counts and timings
    /// only), which the AI-activity poll surfaces without a schema change.
    pub fn latest_completed_event_payload(&self, job_id: i64) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT payload_json FROM ai_job_events
                 WHERE job_id = ?1 AND kind = 'completed'
                 ORDER BY id DESC LIMIT 1",
                params![job_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    // -----------------------------------------------------------------------
    // Outputs
    // -----------------------------------------------------------------------

    /// Upsert a final or partial AI output. The `UNIQUE(scope, owner_id, feature)`
    /// constraint means a later job replaces an earlier one's output.
    ///
    /// No-downgrade rule: a partial result (`complete = 0`) never overwrites an
    /// existing complete one for the same key — a cancelled retry must not
    /// clobber a good cached summary. Enforced by the `WHERE` clause on the
    /// `ON CONFLICT DO UPDATE`: the update applies only when the incoming row
    /// is complete or the existing row is itself partial.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_ai_output(
        &self,
        job_id: i64,
        scope: &str,
        owner_id: &str,
        feature: &str,
        text: &str,
        complete: bool,
        model: Option<&str>,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO ai_outputs
                 (job_id, scope, owner_id, feature, text, complete, model, tokens_in, tokens_out)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(scope, owner_id, feature) DO UPDATE SET
                 job_id     = excluded.job_id,
                 text       = excluded.text,
                 complete   = excluded.complete,
                 model      = excluded.model,
                 tokens_in  = excluded.tokens_in,
                 tokens_out = excluded.tokens_out,
                 created_at = datetime('now')
             WHERE excluded.complete >= ai_outputs.complete",
            params![job_id, scope, owner_id, feature, text, complete, model, tokens_in, tokens_out],
        )?;
        Ok(())
    }

    /// Retrieve a cached AI output by its natural key.
    pub fn get_ai_output(
        &self,
        scope: &str,
        owner_id: &str,
        feature: &str,
    ) -> Result<Option<AiOutputRecord>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT id, job_id, scope, owner_id, feature, text, complete, model,
                        tokens_in, tokens_out
                 FROM ai_outputs WHERE scope = ?1 AND owner_id = ?2 AND feature = ?3",
                params![scope, owner_id, feature],
                |r| {
                    Ok(AiOutputRecord {
                        id: r.get(0)?,
                        job_id: r.get(1)?,
                        scope: r.get(2)?,
                        owner_id: r.get(3)?,
                        feature: r.get(4)?,
                        text: r.get(5)?,
                        complete: r.get::<_, i64>(6)? != 0,
                        model: r.get(7)?,
                        tokens_in: r.get(8)?,
                        tokens_out: r.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------

    /// Set a job to `failed` with the given error info AND force `attempts` to
    /// `max_attempts_ai` so the executor never picks it up again. Used for
    /// terminal errors (ModelMissing, EndpointRefused, Backend) where retrying
    /// would never help.
    pub fn fail_ai_job_terminal(
        &self,
        id: i64,
        max_attempts_ai: i64,
        error_code: &str,
        error_message: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE ai_jobs
             SET status = 'failed', error_code = ?2, error_message = ?3,
                 attempts = ?4, finished_at = datetime('now'), updated_at = datetime('now')
             WHERE id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![id, error_code, error_message, max_attempts_ai],
        )?;
        if changed == 0 {
            guard_exists(&conn, id)?;
        }
        Ok(())
    }

    /// Delete terminal jobs (and their events, via CASCADE) whose `updated_at`
    /// is more than `older_than_days` days in the past. Active (non-terminal)
    /// jobs are never touched. Pass 0 to delete all terminal jobs immediately.
    pub fn prune_ai_history(&self, older_than_days: i64) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        // `<=` intentional: a 0-day prune removes everything terminal (including
        // rows inserted in the same second), avoiding an exclusive-vs-inclusive
        // ambiguity at the boundary. For the production default of 7 days this
        // means rows updated exactly 7 days ago are also pruned, which is fine.
        let changed = conn.execute(
            "DELETE FROM ai_jobs
             WHERE status IN ('succeeded', 'cancelled', 'failed')
               AND updated_at <= datetime('now', '-' || ?1 || ' days')
               -- parked embeds are kept so a model change can retry them
               AND COALESCE(error_code, '') <> 'embed_rejected'",
            params![older_than_days],
        )?;
        Ok(changed)
    }

    /// **Test-only helper**: backdate an AI job's `updated_at` by `hours` hours
    /// so retry-backoff tests don't have to sleep.
    ///
    /// Not for production use; exposed as `pub` so cross-crate tests can call it.
    pub fn backdate_ai_job_updated_at_for_test(&self, id: i64, hours: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE ai_jobs SET updated_at = datetime('now', ?1 || ' hours') WHERE id = ?2",
            params![format!("-{hours}"), id],
        )?;
        Ok(())
    }

    /// List recent AI jobs (newest first), for admin / debug views.
    pub fn list_ai_jobs(&self, limit: i64) -> Result<Vec<AiJobRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, feature, payload_json, priority, status,
                    cancel_requested, model, attempts, error_code, error_message,
                    CAST(strftime('%s', updated_at) AS INTEGER)
             FROM ai_jobs
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], row_to_ai_job)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Message embeddings (S52)
    // -----------------------------------------------------------------------

    /// Store (or replace) the embedding vector for `message_id`. One row per
    /// message — re-embedding (a re-hydrated body, or a newer model) replaces
    /// the existing row rather than accumulating history.
    ///
    /// `vector` is the raw little-endian f32 bytes; this method never formats
    /// it into an error (the only failure mode is the underlying SQL, whose
    /// message cannot contain a bound BLOB parameter's contents).
    pub fn upsert_message_embedding(
        &self,
        message_id: &str,
        model_name: &str,
        dimension: i64,
        vector: &[u8],
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO message_embeddings (message_id, model_name, dimension, vector, created_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(message_id) DO UPDATE SET
                 model_name = excluded.model_name,
                 dimension  = excluded.dimension,
                 vector     = excluded.vector,
                 created_at = excluded.created_at",
            params![message_id, model_name, dimension, vector],
        )?;
        Ok(())
    }

    /// Fetch the stored embedding for a message, if any. Single-row
    /// counterpart to [`Self::get_message_embeddings`] below — kept because
    /// it's the natural shape for a one-message lookup and existing tests
    /// already exercise it; the bulk form is what S53's hybrid search
    /// actually calls.
    pub fn get_message_embedding(
        &self,
        message_id: &str,
    ) -> Result<Option<MessageEmbeddingRecord>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT message_id, model_name, dimension, vector
                 FROM message_embeddings WHERE message_id = ?1",
                params![message_id],
                |r| {
                    Ok(MessageEmbeddingRecord {
                        message_id: r.get(0)?,
                        model_name: r.get(1)?,
                        dimension: r.get(2)?,
                        vector: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Bulk-list every stored embedding among `message_ids`, in no
    /// particular order. A message with no `message_embeddings` row is
    /// simply absent from the result — never an error — which is exactly
    /// the shape S53's hybrid search needs: it asks for this over a
    /// keyword-result set and quietly gets back fewer rows as embedding
    /// coverage thins out, rather than failing. This is the bulk-listing
    /// counterpart that [`Self::get_message_embedding`]'s doc comment above
    /// said had no production caller yet — `Database::search_messages_hybrid`
    /// (repo.rs) is that caller.
    ///
    /// `message_ids` may be empty (returns an empty vec, no query issued).
    ///
    /// Chunks the input into [`EMBEDDINGS_QUERY_CHUNK`]-sized batches
    /// defensively: each batch binds exactly one SQL variable per id, well
    /// under SQLite's ~32766 bound-variable ceiling, regardless of how many
    /// ids a caller passes in. Without this a large enough `message_ids`
    /// (e.g. an unbounded keyword-search result set) could build a single
    /// query with more bound variables than SQLite allows and fail outright
    /// — S53-UI's `find_similar_messages` additionally caps its candidate
    /// scan below the chunk size by construction, but this function chunks
    /// unconditionally so it stays safe for any future caller that doesn't.
    pub fn get_message_embeddings(
        &self,
        message_ids: &[&str],
    ) -> Result<Vec<MessageEmbeddingRecord>, DbError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut out = Vec::with_capacity(message_ids.len());
        for chunk in message_ids.chunks(EMBEDDINGS_QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT message_id, model_name, dimension, vector
                 FROM message_embeddings WHERE message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
                Ok(MessageEmbeddingRecord {
                    message_id: r.get(0)?,
                    model_name: r.get(1)?,
                    dimension: r.get(2)?,
                    vector: r.get(3)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_ai_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<AiJobRecord> {
    Ok(AiJobRecord {
        id: r.get(0)?,
        account_id: r.get(1)?,
        feature: r.get(2)?,
        payload_json: r.get(3)?,
        priority: r.get(4)?,
        status: r.get(5)?,
        cancel_requested: r.get::<_, i64>(6)? != 0,
        model: r.get(7)?,
        attempts: r.get(8)?,
        error_code: r.get(9)?,
        error_message: r.get(10)?,
        updated_at_epoch: r.get(11)?,
    })
}

/// Return an error if `id` does not exist in `ai_jobs`.
fn guard_exists(conn: &rusqlite::Connection, id: i64) -> Result<(), DbError> {
    let exists: bool = conn
        .query_row("SELECT 1 FROM ai_jobs WHERE id = ?1", params![id], |_| {
            Ok(true)
        })
        .optional()?
        .unwrap_or(false);
    if !exists {
        return Err(DbError(format!("unknown ai_job id: {id}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // "Tests are isolated by unique filenames so interference is
        // impossible" is what this comment used to say, and it was false —
        // which is why nobody looked further. Filenames are unique within a
        // process RUN; pids recycle, the counter restarts at 0, and the files
        // were never deleted. A later run reopened an earlier run's database
        // with its rows still in it.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-ai-test-{}-{stamp}-{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn enqueue(db: &Database, feature: &str, priority: i64) -> i64 {
        db.enqueue_ai_job(Some("acc1"), feature, r#"{"message_id":"m1"}"#, priority)
            .unwrap()
    }

    #[test]
    fn ai_job_payload_rejects_content_keys_and_writes_nothing() {
        let db = fresh_db();
        // Every content-shaped key a future feature might be tempted to store.
        for key in [
            "subject",
            "recipients",
            "existing_body",
            "body",
            "prompt",
            "text",
        ] {
            let payload = serde_json::json!({ "instruction": "x", key: "SENTINEL" }).to_string();
            let err = db
                .enqueue_ai_job(Some("acc1"), "draft_message", &payload, 0)
                .expect_err(key);
            assert!(err.0.contains(key), "{key}: {err}");
        }
        // Nested values and non-objects are refused too.
        for payload in [
            r#"{"message_id":{"body":"SENTINEL"}}"#,
            r#"{"instruction":["SENTINEL"]}"#,
            r#"["SENTINEL"]"#,
            "not json",
        ] {
            assert!(
                db.enqueue_ai_job(None, "draft_reply", payload, 0).is_err(),
                "{payload}"
            );
        }
        // Nothing reached the table.
        assert!(db.next_due_ai_job(i64::MAX).unwrap().is_none());
    }

    #[test]
    fn ai_job_payload_accepts_every_allowed_key() {
        let db = fresh_db();
        let payload = serde_json::json!({
            "message_id": "acc1:INBOX:1",
            "thread_id": "t1",
            "instruction": "keep it short",
        })
        .to_string();
        let id = db
            .enqueue_ai_job(Some("acc1"), "draft_reply", &payload, 0)
            .unwrap();
        assert_eq!(db.next_due_ai_job(i64::MAX).unwrap().unwrap().id, id);
        assert!(db.enqueue_ai_job(None, "summarize_thread", "{}", 0).is_ok());
    }

    #[test]
    fn ai_job_enqueue_and_priority_fifo_order() {
        let db = fresh_db();
        // Low priority first (higher number = lower priority).
        let id_bg = enqueue(&db, "summarize_message", 2);
        let id_pf = enqueue(&db, "summarize_message", 1);
        let id_ia = enqueue(&db, "summarize_message", 0); // interactive

        // next_due should return interactive (priority 0) first.
        let next = db.next_due_ai_job(0).unwrap().expect("a job is due");
        assert_eq!(next.id, id_ia, "interactive job dequeued first");

        // Lease it, then the prefetch (priority 1) becomes next.
        db.lease_ai_job(id_ia).unwrap();
        db.set_ai_job_status(id_ia, "succeeded", None, None)
            .unwrap();

        let next = db.next_due_ai_job(0).unwrap().expect("prefetch due");
        assert_eq!(next.id, id_pf);
        db.lease_ai_job(id_pf).unwrap();
        db.set_ai_job_status(id_pf, "succeeded", None, None)
            .unwrap();

        let next = db.next_due_ai_job(0).unwrap().expect("background due");
        assert_eq!(next.id, id_bg);
    }

    #[test]
    fn ai_job_terminal_status_immutable() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);

        db.lease_ai_job(id).unwrap();
        db.set_ai_job_status(id, "succeeded", None, None).unwrap();

        // A late write from a racing executor must not overwrite succeeded.
        db.set_ai_job_status(id, "failed", Some("backend"), Some("late write"))
            .unwrap(); // benign no-op

        let job = db.poll_ai_job(id, 0).unwrap().unwrap().0;
        assert_eq!(job.status, "succeeded");
        assert!(job.error_code.is_none());
    }

    #[test]
    fn ai_cancel_queued_job_is_immediate() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);

        db.request_ai_cancel(id).unwrap();

        let (job, events) = db.poll_ai_job(id, 0).unwrap().expect("job exists");
        assert_eq!(job.status, "cancelled");
        assert!(job.cancel_requested, "cancel_requested should be set");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "cancelled");

        // A cancelled job must not appear as next due.
        assert!(db.next_due_ai_job(0).unwrap().is_none());
    }

    #[test]
    fn ai_cancel_running_job_sets_flag_only() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);
        db.lease_ai_job(id).unwrap();
        db.set_ai_job_status(id, "running", None, None).unwrap();

        db.request_ai_cancel(id).unwrap();

        let (job, events) = db.poll_ai_job(id, 0).unwrap().unwrap();
        assert_eq!(
            job.status, "running",
            "still running — executor must act on flag"
        );
        assert!(job.cancel_requested, "cancel_requested flag is set");
        assert!(
            events.is_empty(),
            "no event yet — executor appends it on cooperative stop"
        );

        // The flag is visible via is_ai_cancel_requested.
        assert!(db.is_ai_cancel_requested(id).unwrap());
    }

    #[test]
    fn ai_events_poll_after_id_returns_only_new() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);

        let e1 = db
            .append_ai_event(id, "started", r#"{"attempt":1}"#)
            .unwrap();
        let e2 = db
            .append_ai_event(id, "delta", r#"{"text":"Hello"}"#)
            .unwrap();
        let _e3 = db
            .append_ai_event(id, "delta", r#"{"text":" world"}"#)
            .unwrap();

        // Poll for events after e1 — should see e2 and e3 only.
        let (_, events) = db.poll_ai_job(id, e1).unwrap().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, e2);
        assert!(events[0].payload_json.contains("Hello"));

        // Poll after e2 — should see only e3.
        let (_, events) = db.poll_ai_job(id, e2).unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].payload_json.contains("world"));
    }

    #[test]
    fn ai_output_upsert_replaces_previous_and_marks_partial() {
        let db = fresh_db();
        let id1 = enqueue(&db, "summarize_message", 0);

        // First upsert: partial result (job cancelled mid-stream).
        db.upsert_ai_output(
            id1,
            "message",
            "m1",
            "summarize_message",
            "partial",
            false,
            Some("llama3.1:8b"),
            Some(100),
            Some(50),
        )
        .unwrap();

        let out = db
            .get_ai_output("message", "m1", "summarize_message")
            .unwrap()
            .unwrap();
        assert!(!out.complete);
        assert_eq!(out.text, "partial");
        assert_eq!(out.job_id, id1);

        // Second job produces a complete result — upsert replaces.
        let id2 = enqueue(&db, "summarize_message", 0);
        db.upsert_ai_output(
            id2,
            "message",
            "m1",
            "summarize_message",
            "complete summary",
            true,
            Some("llama3.1:8b"),
            Some(120),
            Some(80),
        )
        .unwrap();

        let out2 = db
            .get_ai_output("message", "m1", "summarize_message")
            .unwrap()
            .unwrap();
        assert!(out2.complete);
        assert_eq!(out2.text, "complete summary");
        assert_eq!(out2.job_id, id2);
    }

    #[test]
    fn ai_output_partial_never_downgrades_complete() {
        let db = fresh_db();

        // Complete output first.
        let id1 = enqueue(&db, "summarize_message", 0);
        db.upsert_ai_output(
            id1,
            "message",
            "m1",
            "summarize_message",
            "good complete summary",
            true,
            Some("llama3.1:8b"),
            Some(120),
            Some(80),
        )
        .unwrap();

        // A later cancelled retry produces a partial for the same key —
        // it must NOT overwrite the complete output.
        let id2 = enqueue(&db, "summarize_message", 0);
        db.upsert_ai_output(
            id2,
            "message",
            "m1",
            "summarize_message",
            "trunc",
            false,
            Some("llama3.1:8b"),
            Some(30),
            Some(10),
        )
        .unwrap();

        let out = db
            .get_ai_output("message", "m1", "summarize_message")
            .unwrap()
            .unwrap();
        assert!(out.complete, "complete flag preserved");
        assert_eq!(out.text, "good complete summary", "text not downgraded");
        assert_eq!(out.job_id, id1, "job_id still points at the complete run");

        // A new complete output DOES replace the previous complete one.
        let id3 = enqueue(&db, "summarize_message", 0);
        db.upsert_ai_output(
            id3,
            "message",
            "m1",
            "summarize_message",
            "fresher complete summary",
            true,
            Some("llama3.1:8b"),
            Some(130),
            Some(90),
        )
        .unwrap();
        let out = db
            .get_ai_output("message", "m1", "summarize_message")
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "fresher complete summary");
        assert_eq!(out.job_id, id3);
    }

    #[test]
    fn ai_prune_removes_terminal_history_keeps_active() {
        let db = fresh_db();
        let id_active = enqueue(&db, "summarize_message", 0);
        let id_succeeded = enqueue(&db, "summarize_thread", 0);

        // Immediately succeed the second job.
        db.lease_ai_job(id_succeeded).unwrap();
        db.set_ai_job_status(id_succeeded, "succeeded", None, None)
            .unwrap();

        // Prune with 0 days — deletes everything terminal updated ≤ 0 days ago.
        let pruned = db.prune_ai_history(0).unwrap();
        assert_eq!(pruned, 1, "one terminal job pruned");

        // Active job must still be there.
        let jobs = db.list_ai_jobs(100).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id_active);
    }

    #[test]
    fn recover_stale_jobs_requeues_or_fails() {
        let db = fresh_db();
        // A job with few attempts (should be re-queued).
        let id_retry = enqueue(&db, "summarize_message", 0);
        db.lease_ai_job(id_retry).unwrap(); // attempts = 1, status = leased

        // A job that has exhausted attempts (should be terminal-failed).
        let id_exhaust = enqueue(&db, "summarize_message", 0);
        // Manually force attempts to 3 and leave status as running.
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE ai_jobs SET status = 'running', attempts = 3 WHERE id = ?1",
                params![id_exhaust],
            )
            .unwrap();
        }

        let recovered = db.recover_stale_ai_jobs().unwrap();
        assert_eq!(recovered, 2);

        let retry_job = db.poll_ai_job(id_retry, 0).unwrap().unwrap().0;
        assert_eq!(retry_job.status, "queued");

        let exhaust_job = db.poll_ai_job(id_exhaust, 0).unwrap().unwrap().0;
        assert_eq!(exhaust_job.status, "failed");
        assert_eq!(exhaust_job.error_code.as_deref(), Some("backend"));
    }

    // -----------------------------------------------------------------------
    // Message embeddings (S52)
    // -----------------------------------------------------------------------

    #[test]
    fn message_embedding_round_trips_as_little_endian_bytes() {
        let db = fresh_db();
        seed_message_for_embedding(&db, "acc1:INBOX:1");

        let vector: Vec<f32> = vec![0.5, -1.25, 3.0];
        let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        db.upsert_message_embedding("acc1:INBOX:1", "nomic-embed-text", 3, &bytes)
            .unwrap();

        let row = db
            .get_message_embedding("acc1:INBOX:1")
            .unwrap()
            .expect("embedding stored");
        assert_eq!(row.model_name, "nomic-embed-text");
        assert_eq!(row.dimension, 3);
        let decoded: Vec<f32> = row
            .vector
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        assert_eq!(decoded, vector, "vector round-trips exactly");

        // No embedding for an unrelated message.
        assert!(db
            .get_message_embedding("acc1:INBOX:999")
            .unwrap()
            .is_none());
    }

    #[test]
    fn message_embedding_upsert_replaces_previous_vector() {
        let db = fresh_db();
        seed_message_for_embedding(&db, "acc1:INBOX:2");

        db.upsert_message_embedding("acc1:INBOX:2", "model-a", 2, &[0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        db.upsert_message_embedding(
            "acc1:INBOX:2",
            "model-b",
            3,
            &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
        )
        .unwrap();

        let row = db.get_message_embedding("acc1:INBOX:2").unwrap().unwrap();
        assert_eq!(
            row.model_name, "model-b",
            "re-embed replaces, not accumulates"
        );
        assert_eq!(row.dimension, 3);
    }

    #[test]
    fn message_embedding_cascades_with_its_message() {
        let db = fresh_db();
        seed_message_for_embedding(&db, "acc1:INBOX:3");
        db.upsert_message_embedding("acc1:INBOX:3", "model-a", 1, &[0, 0, 0, 0])
            .unwrap();
        assert!(db.get_message_embedding("acc1:INBOX:3").unwrap().is_some());

        {
            let conn = db.lock_conn();
            conn.execute(
                "DELETE FROM messages WHERE id = ?1",
                params!["acc1:INBOX:3"],
            )
            .unwrap();
        }
        assert!(
            db.get_message_embedding("acc1:INBOX:3").unwrap().is_none(),
            "ON DELETE CASCADE removes the embedding with its message"
        );
    }

    #[test]
    fn get_message_embeddings_returns_only_the_rows_that_exist() {
        let db = fresh_db();
        seed_message_for_embedding(&db, "acc1:INBOX:10");
        seed_message_for_embedding(&db, "acc1:INBOX:11");
        seed_message_for_embedding(&db, "acc1:INBOX:12"); // never embedded

        db.upsert_message_embedding("acc1:INBOX:10", "model-a", 2, &[0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:11", "model-a", 2, &[0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();

        // Ask for all three (the partially-embedded-mailbox case): only the
        // two with a row come back, never an error for the third.
        let rows = db
            .get_message_embeddings(&["acc1:INBOX:10", "acc1:INBOX:11", "acc1:INBOX:12"])
            .unwrap();
        let mut ids: Vec<&str> = rows.iter().map(|r| r.message_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["acc1:INBOX:10", "acc1:INBOX:11"]);
    }

    #[test]
    fn get_message_embeddings_of_empty_input_is_empty_not_an_error() {
        let db = fresh_db();
        assert!(db.get_message_embeddings(&[]).unwrap().is_empty());
    }

    #[test]
    fn get_message_embeddings_chunks_past_the_chunk_boundary_without_erroring() {
        // EMBEDDINGS_QUERY_CHUNK + 1 ids spans two chunks (500 + 1); only the
        // very last id has a stored row. If chunking were broken (e.g. a
        // single unchunked query), this would still pass for 501 ids — the
        // real regression this test guards is a caller-supplied list large
        // enough to exceed SQLite's bound-variable ceiling, which we can't
        // literally construct in a unit test, so we instead assert the
        // chunked path returns exactly the expected subset at the boundary.
        let db = fresh_db();
        let ids: Vec<String> = (0..=EMBEDDINGS_QUERY_CHUNK)
            .map(|i| format!("acc1:INBOX:{i}"))
            .collect();
        for id in &ids {
            seed_message_for_embedding(&db, id);
        }
        let last = ids.last().unwrap();
        db.upsert_message_embedding(last, "model-a", 2, &[0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();

        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let rows = db.get_message_embeddings(&refs).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(&rows[0].message_id, last);
    }

    /// The backfill's per-message "has a job?" probes must SEARCH the 0052 index, not
    /// SCAN ai_jobs once per message (0.8 s at 10k messages, 6.8 s at 77k, under the DB lock).
    #[test]
    fn embed_backfill_probes_use_the_ai_jobs_index_not_a_scan() {
        let db = fresh_db();
        let conn = db.lock_conn();
        let mut stmt = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                Database::ENQUEUE_MISSING_EMBED_SQL
            ))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(rusqlite::params!["m", 10_i64, 2_i64], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let ai_jobs_steps: Vec<&String> = plan.iter().filter(|d| d.contains("ai_jobs")).collect();
        assert!(!ai_jobs_steps.is_empty(), "plan: {plan:?}");
        for step in &ai_jobs_steps {
            assert!(
                step.contains("SEARCH") && step.contains("idx_ai_jobs_embed_msg"),
                "ai_jobs must be probed through idx_ai_jobs_embed_msg, got {step:?}; plan: {plan:?}"
            );
        }
    }

    /// Minimal message row so `message_embeddings`' FK is satisfiable.
    fn seed_message_for_embedding(db: &Database, message_id: &str) {
        let conn = db.lock_conn();
        conn.execute(
            "INSERT INTO accounts (id, email, provider) VALUES ('acc1', 'a@x.test', 'imap')
             ON CONFLICT(id) DO NOTHING",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id, account_id, path, name)
             VALUES ('acc1:INBOX', 'acc1', 'INBOX', 'Inbox')
             ON CONFLICT(id) DO NOTHING",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, mailbox_id) VALUES (?1, 'acc1', 'acc1:INBOX')",
            params![message_id],
        )
        .unwrap();
    }

    #[test]
    fn cancel_of_a_failed_job_with_retries_left_sticks() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);
        db.lease_ai_job(id).unwrap();
        db.set_ai_job_status(id, "failed", Some("network"), Some("x"))
            .unwrap();
        db.request_ai_cancel(id).unwrap();
        let (job, events) = db.poll_ai_job(id, 0).unwrap().unwrap();
        assert_eq!(job.status, "cancelled");
        assert!(events.iter().any(|e| e.kind == "cancelled"));
        // Never due again.
        assert!(db.next_due_ai_job(i64::MAX / 2).unwrap().is_none());
    }

    #[test]
    fn stale_recovery_does_not_requeue_a_cancel_requested_job() {
        let db = fresh_db();
        let id = enqueue(&db, "summarize_message", 0);
        db.lease_ai_job(id).unwrap();
        db.request_ai_cancel(id).unwrap(); // leased: sets the flag only
        db.recover_stale_ai_jobs().unwrap();
        let (job, _) = db.poll_ai_job(id, 0).unwrap().unwrap();
        assert_eq!(job.status, "cancelled");
    }

    #[test]
    fn next_due_excluding_skips_the_given_ids() {
        let db = fresh_db();
        let a = enqueue(&db, "summarize_message", 0);
        let b = enqueue(&db, "summarize_message", 0);
        let got = db
            .next_due_ai_job_excluding(i64::MAX / 2, None, None, &[a])
            .unwrap()
            .unwrap();
        assert_eq!(got.id, b);
    }
}
