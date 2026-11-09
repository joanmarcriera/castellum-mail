# AI eval corpus (S51)

Fixed, synthetic-only Catalan/Spanish/English mail fixtures for the
`#[ignore]` regression harness at `../../eval_harness.rs`. **No real mail,
ever** — every message here is invented for this corpus.

## Layout

```
summarize_message/<name>.txt   + <name>.json   — one message per fixture
draft_reply/<name>.txt         + <name>.json   — one original message + instruction per fixture
summarize_thread/<name>/       fixture.json + msg0.txt, msg1.txt, ...
```

`<name>` encodes language and shape: `<lang>_<short|long>[_html]`, e.g.
`es_long_plain`, `en_short_html`. `summarize_message` and `draft_reply` cover
short/long and plain/HTML; `summarize_thread` covers short (2-message) and
long (4-message) threads. Every feature has at least one fixture in each of
`ca`/`es`/`en` — enforced by
`corpus_covers_all_three_features_and_three_languages` in the harness.

## Sidecar schema

`summarize_message` / `draft_reply` (`<name>.json`):

```json
{
  "feature": "summarize_message | draft_reply",
  "language": "ca | es | en",
  "content_format": "plain | html",
  "subject": "...", "from_name": "...", "from_address": "...",
  "content_file": "<name>.txt",
  "instruction": "... (draft_reply only)",
  "rubric": { "...": "see below" }
}
```

`summarize_thread` (`fixture.json`): the same shape but `messages` is an
array, oldest first, each entry adding `sent_at_offset_secs` (seconds after a
fixed fixture epoch — keeps ordering explicit and reproducible),
`rfc822_message_id`, and `references` (the prior messages' rfc822 ids, so
`Database::rebuild_threads` groups them into one thread exactly as a real
sync would).

## Rubric

```json
{
  "min_chars": 20, "max_chars": 400,
  "required_keyword_groups": [["4pm", "4 pm", "16:00"]],
  "forbidden_keywords": []
}
```

Never exact-string matching — the model is non-deterministic. A fixture
scores 0.0–1.0 across four independent checks: length within band, every
`required_keyword_groups` entry present by AT LEAST ONE of its synonyms,
none of the fixture's own `forbidden_keywords` present, and none of the
harness's global refusal markers present (a model that refuses the task is a
regression, checked on every fixture, not just the ones that name it). See
`score_text` in `eval_harness.rs`.

## Extraction (`extraction/<lang>_<name>/`)

`extract_thread_items` fixtures: a thread directory like `summarize_thread`
(`fixture.json` + `msgN.txt`) whose sidecar carries an `expected` block
instead of a keyword `rubric`:

```json
"expected": {
  "action_items": [["signed delivery form", "delivery form"], ["bank transfer"]],
  "dates": ["2026-10-15"],
  "amounts": [{"amount": 1250.0, "currency": "GBP"}]
}
```

Each action item is a group of synonyms matched (accent- and
case-insensitive substring) against the extracted `action_item` texts; dates
match a `date` item's ISO `structured_value`; amounts match currency plus
value within 0.005. Score = recall x precision over dates/amounts, 0..1; a
response the schema validator rejects scores 0. See `score_extraction`, which
is unit-tested on canned JSON (perfect, partial, wrong, malformed) with no
model. At least two fixtures per language are enforced by the corpus
coverage test.

## Score history

With `CASTELLUM_EVAL_JSONL=<file>` the live run appends one line per fixture:
`fixture, feature, language, score, latency_ms, model, sha, ts` (sha from
`CASTELLUM_EVAL_SHA`). Never fixture text or model output. The run ends with
the mean per feature and language and one status line, `EVAL SKIPPED: no
Ollama endpoint` or `EVAL RAN: <n> fixtures, mean <x>`, which the `ai-eval`
CI job copies into the commit-status description.
