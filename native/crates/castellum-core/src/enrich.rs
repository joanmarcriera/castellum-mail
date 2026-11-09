// Castellum - contact enrichment framework (slice 8d).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Trait-based adapter interface so future HTTP sources are a pure Rust addition
// with ZERO UI changes. v1 ships one adapter: `ManualPasteSource`, a heuristic
// parser over pasted profile text. All adapters must be `Send + Sync` so the
// registry is safely shared across threads.
//
// Product constraint: NO automated LinkedIn scraping (ToS). Manual paste only
// for v1; the framework is designed so BYO-key HTTP adapters slot in trivially.

use crate::{CoreError, Profile};
use castellum_db::EnrichmentRow;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors from the enrichment pipeline.
#[derive(Debug)]
pub enum EnrichError {
    EmptyInput,
    UnknownSource(String),
    Other(String),
}

impl std::fmt::Display for EnrichError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnrichError::EmptyInput => write!(f, "empty input"),
            EnrichError::UnknownSource(id) => write!(f, "unknown source: {id}"),
            EnrichError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl From<EnrichError> for CoreError {
    fn from(e: EnrichError) -> Self {
        CoreError::Db(e.to_string())
    }
}

// ── Trait ────────────────────────────────────────────────────────────────────

/// Whether the source needs interactive user input (paste/upload) or can fetch
/// autonomously given an API key.
pub enum SourceKind {
    PasteImport,
    Http,
}

/// One enrichment field returned by a source adapter.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnrichedField {
    pub field: String,
    pub value: String,
    pub provenance: String,
}

/// An enrichment adapter. Implementors are registered in [`sources()`] and
/// driven generically by the UI — adding a new adapter requires no UI changes.
pub trait EnrichmentSource: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn kind(&self) -> SourceKind;
    /// Short label shown above the input field in the "Add info…" sheet.
    fn input_label(&self) -> &'static str;
    /// Parse `input` and return enrichment fields. `fetched_at` is the current
    /// Unix epoch seconds, used to format the provenance timestamp.
    /// Only `EnrichError::EmptyInput` is a hard error; weird input → `summary`.
    fn enrich(&self, input: &str, fetched_at: i64) -> Result<Vec<EnrichedField>, EnrichError>;
}

// ── ManualPasteSource ────────────────────────────────────────────────────────

/// Heuristic parser over pasted profile text (the v1 "paste adapter").
///
/// Rules:
/// - A line containing `linkedin.com/in/` → `linkedin_url` field.
/// - The first line matching `"X at Y"` or `"X @ Y"` → `title` + `company`.
/// - The first short line (< 40 chars, contains a comma) seen after the
///   headline line → `location`.
/// - All remaining non-empty text (trimmed, capped at 2000 chars) → `summary`.
/// - Empty input → `EnrichError::EmptyInput` (the only error this emits).
pub struct ManualPasteSource;

impl EnrichmentSource for ManualPasteSource {
    fn id(&self) -> &'static str {
        "manual"
    }

    fn display_name(&self) -> &'static str {
        "Manual paste"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::PasteImport
    }

    fn input_label(&self) -> &'static str {
        "Paste profile text or URL"
    }

    fn enrich(&self, input: &str, fetched_at: i64) -> Result<Vec<EnrichedField>, EnrichError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(EnrichError::EmptyInput);
        }

        let provenance = epoch_to_date_provenance(fetched_at);
        let mut fields: Vec<EnrichedField> = Vec::new();
        let mut headline_line_idx: Option<usize> = None;
        let mut location_found = false;
        let mut summary_parts: Vec<&str> = Vec::new();

        // Single-pass over lines.
        for (idx, line) in trimmed.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // LinkedIn URL (case-insensitive: "LinkedIn.com/in/…" pastes are
            // common) — deduplicate if we find it multiple times.
            if find_ascii_ci(line, "linkedin.com/in/").is_some()
                && !fields.iter().any(|f| f.field == "linkedin_url")
            {
                // Extract the URL (the line may have surrounding text).
                let url = extract_linkedin_url(line);
                fields.push(EnrichedField {
                    field: "linkedin_url".to_string(),
                    value: url,
                    provenance: provenance.clone(),
                });
                continue;
            }

            // Headline: "X at Y" or "X @ Y". Take the first matching line.
            if headline_line_idx.is_none() {
                if let Some((title, company)) = parse_headline(line) {
                    fields.push(EnrichedField {
                        field: "title".to_string(),
                        value: title,
                        provenance: provenance.clone(),
                    });
                    fields.push(EnrichedField {
                        field: "company".to_string(),
                        value: company,
                        provenance: provenance.clone(),
                    });
                    headline_line_idx = Some(idx);
                    continue;
                }
            }

            // Location heuristic: short line (< 40 chars, has comma) immediately
            // after the headline.
            if !location_found {
                if let Some(hi) = headline_line_idx {
                    if idx > hi && line.len() < 40 && line.contains(',') {
                        fields.push(EnrichedField {
                            field: "location".to_string(),
                            value: line.to_string(),
                            provenance: provenance.clone(),
                        });
                        location_found = true;
                        continue;
                    }
                }
            }

            // Everything else feeds the summary.
            summary_parts.push(line);
        }

        // Assemble summary (skip if empty). Cap at 2000 CHARS (not bytes):
        // byte-slicing a &str panics on multibyte boundaries (CJK/emoji paste).
        let summary_raw = summary_parts.join("\n");
        let summary = summary_raw.trim();
        if !summary.is_empty() {
            let capped: String = summary.chars().take(2000).collect();
            fields.push(EnrichedField {
                field: "summary".to_string(),
                value: capped,
                provenance: provenance.clone(),
            });
        }

        Ok(fields)
    }
}

/// ASCII case-insensitive substring search. Returns the byte offset of the
/// first match. Safe for slicing the original string: the needle is ASCII, so
/// any matching haystack bytes are ASCII too and the offset is a char boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Extract the best linkedin URL from a line that is known to contain
/// `linkedin.com/in/` (matched case-insensitively; the extracted value keeps
/// the original casing). Returns the URL portion or the full line as fallback.
fn extract_linkedin_url(line: &str) -> String {
    // Find the start of "https://" or "http://" or bare "linkedin.com" in the line.
    if let Some(pos) = find_ascii_ci(line, "https://") {
        let rest = &line[pos..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    if let Some(pos) = find_ascii_ci(line, "http://") {
        let rest = &line[pos..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    // Bare "linkedin.com/in/…" without a scheme.
    if let Some(pos) = find_ascii_ci(line, "linkedin.com/in/") {
        let rest = &line[pos..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return format!("https://{}", &rest[..end]);
    }
    line.trim().to_string()
}

/// Try to parse a headline of the form `"X at Y"` or `"X @ Y"`.
/// Returns `(title, company)` on match, or `None`.
fn parse_headline(line: &str) -> Option<(String, String)> {
    // Case-insensitive split on " at " (with surrounding spaces).
    let lower = line.to_lowercase();
    for sep in [" at ", " @ "] {
        if let Some(pos) = lower.find(sep) {
            let title = line[..pos].trim().to_string();
            let company = line[pos + sep.len()..].trim().to_string();
            if !title.is_empty() && !company.is_empty() {
                return Some((title, company));
            }
        }
    }
    None
}

/// Format `fetched_at` (Unix epoch seconds) as `"pasted YYYY-MM-DD"`.
fn epoch_to_date_provenance(epoch: i64) -> String {
    // Compute year/month/day using the Gregorian calendar algorithm (no chrono dep).
    let days = epoch / 86_400;
    let (y, m, d) = days_to_ymd(days);
    format!("pasted {y:04}-{m:02}-{d:02}")
}

/// Convert a count of days since 1970-01-01 to (year, month, day).
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html (civil_from_days)
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Registry ─────────────────────────────────────────────────────────────────

static MANUAL: ManualPasteSource = ManualPasteSource;
static SOURCES_SLICE: &[&dyn EnrichmentSource] = &[&MANUAL as &dyn EnrichmentSource];

/// All registered enrichment sources. Adding a new adapter = one line here.
pub fn sources() -> &'static [&'static dyn EnrichmentSource] {
    SOURCES_SLICE
}

fn find_source(source_id: &str) -> Option<&'static dyn EnrichmentSource> {
    sources().iter().find(|s| s.id() == source_id).copied()
}

// ── Profile JSON methods ─────────────────────────────────────────────────────

/// Wire shape returned by `list_enrichment_sources_json`.
#[derive(Serialize)]
struct SourceInfoJson {
    id: &'static str,
    display_name: &'static str,
    kind: &'static str,
    input_label: &'static str,
}

/// Wire shape returned by `enrich_contact_json(save: false)` — a PREVIEW row.
/// This is the FFI contract for the preview path: exactly `field`, `value`,
/// `provenance` (no `id`/`source`/`fetched_at`, which only exist once saved).
/// Swift decodes this with its own `PreviewEnrichmentField` DTO — keep in sync.
#[derive(Serialize)]
struct PreviewFieldOut {
    field: String,
    value: String,
    provenance: String,
}

/// Wire shape used inside `get_contact_json` for enrichment rows.
/// Excludes `contact_id` (redundant in the detail response).
#[derive(Serialize)]
pub(crate) struct EnrichmentFieldOut {
    pub id: i64,
    pub source: String,
    pub field: String,
    pub value: String,
    pub provenance: Option<String>,
    pub fetched_at: i64,
}

impl From<EnrichmentRow> for EnrichmentFieldOut {
    fn from(r: EnrichmentRow) -> Self {
        EnrichmentFieldOut {
            id: r.id,
            source: r.source,
            field: r.field,
            value: r.value,
            provenance: r.provenance,
            fetched_at: r.fetched_at,
        }
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn to_json<T: Serialize>(v: &T) -> Result<String, CoreError> {
    serde_json::to_string(v).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

/// Dispatch request decoded from `castellum_enrichment` op_json.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EnrichmentOp {
    ListSources,
    Enrich {
        contact_id: i64,
        source_id: String,
        input: String,
        #[serde(default)]
        save: bool,
    },
    Delete {
        id: i64,
    },
    /// Direct upsert of one field+value bypassing the heuristic parser.
    /// Used by the quick-add row in the UI (source = "manual" typically).
    RawAdd {
        contact_id: i64,
        source: String,
        field: String,
        value: String,
        provenance: Option<String>,
    },
}

impl Profile {
    /// JSON array of all registered enrichment sources.
    pub fn list_enrichment_sources_json(&self) -> Result<String, CoreError> {
        let out: Vec<SourceInfoJson> = sources()
            .iter()
            .map(|s| SourceInfoJson {
                id: s.id(),
                display_name: s.display_name(),
                kind: match s.kind() {
                    SourceKind::PasteImport => "PasteImport",
                    SourceKind::Http => "Http",
                },
                input_label: s.input_label(),
            })
            .collect();
        to_json(&out)
    }

    /// Parse `input` through the named `source_id` adapter.
    ///
    /// The two modes return DIFFERENT JSON shapes:
    /// - `save = false` → preview: `[PreviewFieldOut]` (`field`,`value`,
    ///   `provenance` only — no DB ids), nothing written to the DB.
    /// - `save = true`  → parse + upsert, returns the saved rows as
    ///   `[EnrichmentFieldOut]` (with `id`, `source`, `fetched_at`).
    ///
    /// Saving is ADDITIVE per (source, field): a second paste that parses to
    /// fewer fields does NOT remove previously saved fields — per-field delete
    /// is the correction path (see `upsert_enrichment_fields`).
    pub fn enrich_contact_json(
        &self,
        contact_id: i64,
        source_id: &str,
        input: &str,
        save: bool,
    ) -> Result<String, CoreError> {
        let source = find_source(source_id)
            .ok_or_else(|| EnrichError::UnknownSource(source_id.to_string()))?;
        let fetched_at = now_epoch();
        let enriched = source.enrich(input, fetched_at).map_err(CoreError::from)?;

        if save {
            let rows: Vec<(String, String, Option<String>)> = enriched
                .iter()
                .map(|f| (f.field.clone(), f.value.clone(), Some(f.provenance.clone())))
                .collect();
            self.db
                .upsert_enrichment_fields(contact_id, source_id, &rows, fetched_at)
                .map_err(db_io)?;
            // Return the saved rows from the DB (canonical ids assigned).
            let saved = self.db.list_enrichment(contact_id).map_err(db_io)?;
            let out: Vec<EnrichmentFieldOut> = saved
                .into_iter()
                .filter(|r| r.source == source_id)
                .map(EnrichmentFieldOut::from)
                .collect();
            to_json(&out)
        } else {
            // Preview: parsed fields only — no DB ids yet (see PreviewFieldOut).
            let preview: Vec<PreviewFieldOut> = enriched
                .into_iter()
                .map(|f| PreviewFieldOut {
                    field: f.field,
                    value: f.value,
                    provenance: f.provenance,
                })
                .collect();
            to_json(&preview)
        }
    }

    /// Delete a single enrichment row by id.
    pub fn delete_enrichment(&self, id: i64) -> Result<(), CoreError> {
        self.db.delete_enrichment(id).map_err(db_io)
    }

    /// List all enrichment rows for `contact_id` as a JSON array.
    pub fn list_enrichment_json(&self, contact_id: i64) -> Result<String, CoreError> {
        let rows = self.db.list_enrichment(contact_id).map_err(db_io)?;
        let out: Vec<EnrichmentFieldOut> = rows.into_iter().map(EnrichmentFieldOut::from).collect();
        to_json(&out)
    }

    /// Dispatch an enrichment operation from the FFI JSON-dispatch surface.
    /// `op_json` must decode to one of `{op:"list_sources"}`,
    /// `{op:"enrich", contact_id, source_id, input, save?}`,
    /// `{op:"delete", id}`, or
    /// `{op:"raw_add", contact_id, source, field, value, provenance?}`.
    pub fn enrichment_dispatch_json(&self, op_json: &str) -> Result<String, CoreError> {
        let op: EnrichmentOp = serde_json::from_str(op_json)
            .map_err(|e| CoreError::Db(format!("bad enrichment op: {e}")))?;
        match op {
            EnrichmentOp::ListSources => self.list_enrichment_sources_json(),
            EnrichmentOp::Enrich {
                contact_id,
                source_id,
                input,
                save,
            } => self.enrich_contact_json(contact_id, &source_id, &input, save),
            EnrichmentOp::Delete { id } => {
                self.delete_enrichment(id)?;
                Ok("{}".to_string())
            }
            EnrichmentOp::RawAdd {
                contact_id,
                source,
                field,
                value,
                provenance,
            } => {
                let fetched_at = now_epoch();
                self.db
                    .upsert_enrichment_fields(
                        contact_id,
                        &source,
                        &[(field, value, provenance)],
                        fetched_at,
                    )
                    .map_err(db_io)?;
                Ok("{}".to_string())
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_profile;

    fn profile(tag: &str) -> Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-enrich-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    // ── ManualPasteSource parsing ────────────────────────────────────────────

    #[test]
    fn manual_paste_parses_linkedin_url() {
        let src = ManualPasteSource;
        let input = "Check my profile at https://linkedin.com/in/jdoe\nSenior Engineer at Acme";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        let linkedin = fields.iter().find(|f| f.field == "linkedin_url").unwrap();
        assert_eq!(linkedin.value, "https://linkedin.com/in/jdoe");
        assert!(linkedin.provenance.starts_with("pasted "));
    }

    #[test]
    fn manual_paste_parses_title_at_company() {
        let src = ManualPasteSource;
        let input = "Jane Smith at Contoso Ltd\nCambridge, UK\nSome other text here";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        let title = fields.iter().find(|f| f.field == "title").unwrap();
        let company = fields.iter().find(|f| f.field == "company").unwrap();
        assert_eq!(title.value, "Jane Smith");
        assert_eq!(company.value, "Contoso Ltd");
    }

    #[test]
    fn manual_paste_parses_title_at_company_with_at_symbol() {
        let src = ManualPasteSource;
        let input = "Lead Developer @ MegaCorp\nFull stack engineer";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        let title = fields.iter().find(|f| f.field == "title").unwrap();
        let company = fields.iter().find(|f| f.field == "company").unwrap();
        assert_eq!(title.value, "Lead Developer");
        assert_eq!(company.value, "MegaCorp");
    }

    #[test]
    fn manual_paste_location_heuristic() {
        let src = ManualPasteSource;
        // Short comma-containing line directly after headline should be location.
        let input = "Alice Smith at BigCorp\nCambridge, UK\nHere is some remaining bio text.";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        let loc = fields.iter().find(|f| f.field == "location").unwrap();
        assert_eq!(loc.value, "Cambridge, UK");
    }

    #[test]
    fn unparseable_input_yields_summary_only() {
        let src = ManualPasteSource;
        // No LinkedIn URL, no "X at Y", no location pattern — everything goes to summary.
        let input = "Just a bunch of random text with no recognisable structure whatsoever.";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        assert!(!fields.is_empty(), "should produce at least summary");
        assert!(fields.iter().all(|f| f.field == "summary"));
        let summary = fields.iter().find(|f| f.field == "summary").unwrap();
        assert!(summary.value.contains("random text"));
    }

    #[test]
    fn empty_input_errors() {
        let src = ManualPasteSource;
        assert!(matches!(src.enrich("", 0), Err(EnrichError::EmptyInput)));
        assert!(matches!(
            src.enrich("   \n\t  ", 0),
            Err(EnrichError::EmptyInput)
        ));
    }

    #[test]
    fn summary_capped_at_2000_chars() {
        let src = ManualPasteSource;
        let long = "x".repeat(3000);
        let fields = src.enrich(&long, 0).unwrap();
        let summary = fields.iter().find(|f| f.field == "summary").unwrap();
        assert!(summary.value.chars().count() <= 2000);
    }

    #[test]
    fn multibyte_summary_does_not_panic_and_caps_by_chars() {
        // Regression: `&summary[..2000]` byte-sliced a &str, panicking on any
        // multibyte char at the boundary. Push 3000 CJK chars (3 bytes each)
        // through the FULL enrich path (parse + save + read back).
        let p = profile("multibyte");
        let c = p.db.create_contact("cjk@x.test", None).unwrap();
        let cjk: String = "日".repeat(3000);
        let saved = p.enrich_contact_json(c.id, "manual", &cjk, true).unwrap();
        assert!(saved.contains("summary"), "{saved}");
        let rows = p.db.list_enrichment(c.id).unwrap();
        let summary = rows.iter().find(|r| r.field == "summary").unwrap();
        assert_eq!(summary.value.chars().count(), 2000, "capped at 2000 chars");
        assert!(summary.value.chars().all(|ch| ch == '日'), "no mangling");
        // Emoji (4-byte chars) crossing the cap boundary must also survive.
        let src = ManualPasteSource;
        let fields = src.enrich(&"🚀".repeat(3000), 0).unwrap();
        let s = fields.iter().find(|f| f.field == "summary").unwrap();
        assert_eq!(s.value.chars().count(), 2000);
    }

    #[test]
    fn linkedin_url_match_is_case_insensitive() {
        let src = ManualPasteSource;
        let input = "See https://www.LinkedIn.com/in/JDoe for more";
        let fields = src.enrich(input, 1_751_500_000).unwrap();
        let li = fields.iter().find(|f| f.field == "linkedin_url").unwrap();
        // Match is case-insensitive; extracted value keeps original casing.
        assert_eq!(li.value, "https://www.LinkedIn.com/in/JDoe");

        // Bare (schemeless) uppercase variant.
        let fields2 = src.enrich("LINKEDIN.COM/IN/jane", 0).unwrap();
        let li2 = fields2.iter().find(|f| f.field == "linkedin_url").unwrap();
        assert_eq!(li2.value, "https://LINKEDIN.COM/IN/jane");
    }

    #[test]
    fn preview_json_shape_is_field_value_provenance_only() {
        // FFI contract: save:false rows have EXACTLY {field, value, provenance}.
        // Swift's PreviewEnrichmentField decoder depends on this shape —
        // the saved shape (id/source/fetched_at) must NOT leak into previews.
        let p = profile("preview_shape");
        let c = p.db.create_contact("shape@x.test", None).unwrap();
        let out = p
            .enrich_contact_json(c.id, "manual", "Ann Lee at Initech\nBio", false)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let rows = parsed.as_array().expect("array");
        assert!(!rows.is_empty());
        for row in rows {
            let obj = row.as_object().expect("object");
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["field", "provenance", "value"],
                "preview row must have exactly field/value/provenance: {row}"
            );
        }
    }

    #[test]
    fn get_contact_json_includes_enrichment_key() {
        let p = profile("detail_enrichment");
        let c = p.db.create_contact("detail@x.test", None).unwrap();
        // Empty case: key present with an empty array.
        let empty = p.get_contact_json(c.id).unwrap();
        assert!(empty.contains("\"enrichment\":[]"), "{empty}");
        // Populated case.
        p.enrich_contact_json(c.id, "manual", "Zed at Initech", true)
            .unwrap();
        let full = p.get_contact_json(c.id).unwrap();
        assert!(full.contains("\"enrichment\":["), "{full}");
        assert!(full.contains("\"field\":\"title\""), "{full}");
        assert!(full.contains("\"email\":\"detail@x.test\""), "{full}");
    }

    // ── Profile JSON dispatch ────────────────────────────────────────────────

    #[test]
    fn unknown_source_id_errors() {
        let p = profile("unknown_src");
        let c = p.db.create_contact("test@x.test", None).unwrap();
        let err = p.enrich_contact_json(c.id, "nonexistent", "text", false);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent"),
            "error should name the bad source id: {msg}"
        );
    }

    #[test]
    fn preview_does_not_save_save_does() {
        let p = profile("preview_save");
        let c = p.db.create_contact("test2@x.test", None).unwrap();

        // Preview: parse but do NOT write.
        let preview = p
            .enrich_contact_json(c.id, "manual", "Alice Smith at Acme\nBio text", false)
            .unwrap();
        assert!(preview.contains("title") || preview.contains("summary"));

        // Nothing should be in the DB yet.
        let before = p.db.list_enrichment(c.id).unwrap();
        assert!(before.is_empty(), "preview must not write to DB");

        // Save: same input, this time write.
        let saved_json = p
            .enrich_contact_json(c.id, "manual", "Alice Smith at Acme\nBio text", true)
            .unwrap();
        assert!(saved_json.contains("title") || saved_json.contains("summary"));

        let after = p.db.list_enrichment(c.id).unwrap();
        assert!(!after.is_empty(), "save=true must write rows");
    }

    #[test]
    fn list_sources_json_roundtrip() {
        let p = profile("sources_json");
        let json = p.list_enrichment_sources_json().unwrap();
        assert!(json.contains("\"id\":\"manual\""), "{json}");
        assert!(json.contains("\"display_name\""), "{json}");
        assert!(json.contains("\"input_label\""), "{json}");
    }

    #[test]
    fn delete_enrichment_removes_row() {
        let p = profile("delete_row");
        let c = p.db.create_contact("del@x.test", None).unwrap();
        // Use input that produces multiple rows so we can verify targeted deletion.
        p.enrich_contact_json(c.id, "manual", "Test at Corp\nSome bio", true)
            .unwrap();
        let rows = p.db.list_enrichment(c.id).unwrap();
        assert!(rows.len() >= 2, "expected title + company + summary rows");
        let id = rows[0].id;
        let count_before = rows.len();
        p.delete_enrichment(id).unwrap();
        let after = p.db.list_enrichment(c.id).unwrap();
        assert_eq!(after.len(), count_before - 1, "exactly one row removed");
        assert!(
            !after.iter().any(|r| r.id == id),
            "deleted id no longer present"
        );
    }

    #[test]
    fn dispatch_roundtrip_list_sources() {
        let p = profile("dispatch_list");
        let out = p
            .enrichment_dispatch_json(r#"{"op":"list_sources"}"#)
            .unwrap();
        assert!(out.starts_with('['), "expected JSON array: {out}");
        assert!(out.contains("manual"), "{out}");
    }

    #[test]
    fn dispatch_roundtrip_enrich_preview() {
        let p = profile("dispatch_enrich");
        let c = p.db.create_contact("dp@x.test", None).unwrap();
        let op = serde_json::json!({
            "op": "enrich",
            "contact_id": c.id,
            "source_id": "manual",
            "input": "Bob Jones at Megacorp",
            "save": false
        })
        .to_string();
        let out = p.enrichment_dispatch_json(&op).unwrap();
        assert!(out.contains("title") || out.contains("company"), "{out}");
        assert!(
            p.db.list_enrichment(c.id).unwrap().is_empty(),
            "preview must not save"
        );
    }

    #[test]
    fn dispatch_roundtrip_delete() {
        let p = profile("dispatch_delete");
        let c = p.db.create_contact("dd@x.test", None).unwrap();
        p.enrich_contact_json(c.id, "manual", "Carol at Corp", true)
            .unwrap();
        let rows = p.db.list_enrichment(c.id).unwrap();
        assert!(!rows.is_empty());
        let id = rows[0].id;
        let count_before = rows.len();
        let out = p
            .enrichment_dispatch_json(&format!(r#"{{"op":"delete","id":{id}}}"#))
            .unwrap();
        assert_eq!(out, "{}");
        let after = p.db.list_enrichment(c.id).unwrap();
        assert_eq!(
            after.len(),
            count_before - 1,
            "one row removed via dispatch"
        );
        assert!(!after.iter().any(|r| r.id == id), "deleted id gone");
    }

    #[test]
    fn dispatch_invalid_op_errors() {
        let p = profile("dispatch_invalid");
        assert!(p.enrichment_dispatch_json(r#"{"op":"explode"}"#).is_err());
        assert!(p.enrichment_dispatch_json("not json").is_err());
    }

    #[test]
    fn epoch_to_date_provenance_correct() {
        // 2026-07-03 00:00:00 UTC = 1751500800 (approx — verify the exact epoch)
        // Using a well-known date: 2000-01-01 = 946684800
        let prov = epoch_to_date_provenance(946_684_800);
        assert_eq!(prov, "pasted 2000-01-01");
        // Unix epoch day 0 = 1970-01-01.
        let prov0 = epoch_to_date_provenance(0);
        assert_eq!(prov0, "pasted 1970-01-01");
    }
}
