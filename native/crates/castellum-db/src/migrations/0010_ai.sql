-- 0010: AI job queue (slice 7a).
--
-- Three tables drive the local-AI feature set:
--
--   ai_jobs       — durable queue of AI requests (summarise/draft).
--   ai_job_events — streaming log for progressive UI rendering (deltas).
--   ai_outputs    — persisted final (or partial) text results.
--
-- Lifecycle note: ai_outputs rows intentionally OUTLIVE their ai_jobs rows.
-- They are the cached summaries/drafts shown in the UI; pruning job history
-- must not destroy them. Growth is bounded by UNIQUE(scope, owner_id, feature)
-- — at most one row per entity/feature pair, not one per job.
--
-- Design deviations from project-docs/archive/electron/architecture-review/07-ai-architecture.md:
--   * Integer PRIMARY KEY (matches `operations` table parity; arch doc used TEXT UUIDs).
--   * `cancel_requested` flag added: lets the executor honour cooperative cancellation
--     without polling a separate table.
--   * `attempts` column added to ai_jobs: drives the retry/cap logic inside the executor.
--   * Full 9-status CHECK kept though v1 reaches only 7 statuses — kept for forward compat.
--   * `payload_json` holds message/thread IDs (refs) ONLY; actual content is never stored.
--   * `error_message` carries typed variant info only (model name, hostname); NEVER prompt
--     text or body excerpts — enforced at the repo/executor boundary.
--
-- This migration is greenfield (no existing AI rows); no backfill required.

CREATE TABLE ai_jobs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id       TEXT,
    feature          TEXT NOT NULL,
    -- feature values: summarize_message | summarize_thread | draft_reply
    payload_json     TEXT NOT NULL,
    -- refs only: {message_id} / {thread_id} / {message_id, instruction}
    -- NEVER carries message content, subject text, or body excerpts.
    -- DOCUMENTED EXCEPTION: `instruction` (draft_reply) is user-typed intent
    -- ("reply declining politely"), stored so the prompt can be rebuilt for a
    -- crash-safe retry. It is user input about the reply, not email content,
    -- and must still never leak into error_message or event payloads.
    priority         INTEGER NOT NULL DEFAULT 0,
    -- 0 interactive, 1 prefetch, 2 background, 3 maintenance (ASC = highest priority first)
    status           TEXT NOT NULL DEFAULT 'queued'
                     CHECK(status IN ('queued','leased','running','streaming',
                                      'succeeded','failed','cancelled',
                                      'paused_policy','paused_resource')),
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    model            TEXT,
    attempts         INTEGER NOT NULL DEFAULT 0,
    error_code       TEXT,
    -- error_code values: model_missing | endpoint_refused | network | timeout | backend | cancelled
    error_message    TEXT,
    -- metadata only (model name, hostname); NEVER prompt text or body content
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    started_at       TEXT,
    finished_at      TEXT
);
CREATE INDEX idx_ai_jobs_pending ON ai_jobs(status, priority, id);

CREATE TABLE ai_job_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id       INTEGER NOT NULL REFERENCES ai_jobs(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    -- kind values: started | delta | completed | failed | cancelled
    payload_json TEXT NOT NULL,
    -- delta:   {"text":"..."}
    -- started: {"attempt": n}
    -- others:  {}
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_ai_job_events_poll ON ai_job_events(job_id, id);

CREATE TABLE ai_outputs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id       INTEGER NOT NULL,
    scope        TEXT NOT NULL,
    -- scope values: message | thread | draft
    owner_id     TEXT NOT NULL,
    feature      TEXT NOT NULL,
    text         TEXT NOT NULL,
    complete     INTEGER NOT NULL DEFAULT 1,
    -- 0 = partial (persisted on cancel or mid-stream failure)
    model        TEXT,
    tokens_in    INTEGER,
    tokens_out   INTEGER,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(scope, owner_id, feature)
);
