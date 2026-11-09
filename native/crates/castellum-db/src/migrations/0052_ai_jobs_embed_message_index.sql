-- 0052 (2871, embeddings backfill): `enqueue_missing_embed_jobs` asks "does this
-- message already have an embed job?" once per message. The message id lives only
-- inside payload_json, so without an index every probe scans all of ai_jobs
-- (seconds at tens of thousands of messages, under the DB lock). Partial
-- expression index over embed jobs; the query compares with `= +m.id` (unary plus
-- gives the TEXT affinity the index expression has). Index only: no data change.
CREATE INDEX IF NOT EXISTS idx_ai_jobs_embed_msg
    ON ai_jobs (json_extract(payload_json, '$.message_id'), status)
    WHERE feature = 'embed_message';
