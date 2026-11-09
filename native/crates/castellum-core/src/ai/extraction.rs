// Castellum - thread-extraction JSON schema (S54).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The typed shape `defense::validate_output`'s `OutputSchema::Json` arm
// validates the `extract_thread_items` model response against, and the
// canonical form persisted to `ai_outputs.text` / re-parsed by
// `run::run_ai_jobs_with_backend_at` into `thread_extractions` rows.
//
// Deliberately hand-rolled against `serde_json::Value` rather than a direct
// `#[derive(Deserialize)]` on `ExtractedItem`: serde's own parse/type errors
// can echo fragments of the offending input in their `Display` text, and
// `OutputRejected` must never carry model output (see `defense.rs`'s
// privacy-contract doc comment and `run.rs:8-18`). Walking the `Value` tree
// by hand means every rejection reason is a fixed `&'static str` we wrote —
// never a byte of the untrusted response.

use crate::ai::defense::neutralise;
use serde::Serialize;
use serde_json::Value;

/// Hard cap on how many items one extraction response may contain. A model
/// that runs away enumerating hundreds of "items" is a malfunction, not a
/// thread with 500 action items — reject rather than let it swamp the panel.
pub const MAX_ITEMS: usize = 200;

/// Hard cap on one item's `text` length (chars). Generous for a quoted
/// sentence or two; a model that dumps a whole paragraph per item has
/// stopped extracting and started summarising — reject that shape too.
pub const MAX_ITEM_TEXT_CHARS: usize = 2_000;

/// One validated extraction item. Serializes to the canonical JSON shape
/// persisted in `ai_outputs.text` and shown (via `thread_extractions`) in
/// the Swift panel: `{"kind":"...", "text":"...", "structured_value":{...}?}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtractedItem {
    /// One of `action_item` | `date` | `amount` — checked by
    /// [`parse_and_validate`]; never any other value.
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_value: Option<StructuredValue>,
}

/// A `date` item's ISO date, or an `amount` item's amount + currency.
/// Untagged: the two shapes have disjoint field sets, so no external tag is
/// needed, and the persisted JSON reads as the plain object a schema
/// document would describe rather than a Rust-enum-shaped wrapper.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum StructuredValue {
    Date { date: String },
    Amount { amount: f64, currency: String },
}

/// Parse and validate a model's raw `extract_thread_items` response against
/// the fixed schema: a JSON array of objects, each `kind` one of
/// `action_item`/`date`/`amount`, non-empty `text`, and an optional
/// `structured_value` matching `kind` (a `date` object for `date`, an
/// `amount`+`currency` object for `amount`; `action_item` carries none).
///
/// Returns `Err(reason)` — a fixed, content-free string — for anything that
/// doesn't match, INCLUDING invalid JSON; never panics on adversarial input
/// (every `Value` accessor here returns `Option`/`Result`, no `unwrap`/
/// indexing that could panic on a malformed shape).
pub fn parse_and_validate(raw: &str) -> Result<Vec<ExtractedItem>, &'static str> {
    let value: Value = serde_json::from_str(raw).map_err(|_| "response is not valid JSON")?;
    let arr = value
        .as_array()
        .ok_or("response must be a JSON array of items")?;
    if arr.len() > MAX_ITEMS {
        return Err("response has too many items");
    }

    let mut items = Vec::with_capacity(arr.len());
    for entry in arr {
        let obj = entry.as_object().ok_or("item is not a JSON object")?;

        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("item is missing a string \"kind\"")?;
        if !matches!(kind, "action_item" | "date" | "amount") {
            return Err("item \"kind\" is not action_item/date/amount");
        }

        let raw_text = obj
            .get("text")
            .and_then(Value::as_str)
            .ok_or("item is missing a string \"text\"")?;
        let text = neutralise(raw_text.trim());
        if text.is_empty() {
            return Err("item \"text\" is empty");
        }
        if text.chars().count() > MAX_ITEM_TEXT_CHARS {
            return Err("item \"text\" is too long");
        }

        let structured_value = match obj.get("structured_value") {
            None | Some(Value::Null) => None,
            Some(Value::Object(sv)) => Some(parse_structured_value(kind, sv)?),
            Some(_) => return Err("item \"structured_value\" must be an object or null"),
        };

        items.push(ExtractedItem {
            kind: kind.to_string(),
            text,
            structured_value,
        });
    }
    Ok(items)
}

fn parse_structured_value(
    kind: &str,
    sv: &serde_json::Map<String, Value>,
) -> Result<StructuredValue, &'static str> {
    match kind {
        "date" => {
            let date = sv
                .get("date")
                .and_then(Value::as_str)
                .ok_or("date item's structured_value is missing \"date\"")?;
            if !is_plausible_iso_date(date) {
                return Err("date item's structured \"date\" is not YYYY-MM-DD");
            }
            Ok(StructuredValue::Date {
                date: neutralise(date),
            })
        }
        "amount" => {
            let amount = sv
                .get("amount")
                .and_then(Value::as_f64)
                .ok_or("amount item's structured_value is missing a numeric \"amount\"")?;
            if !amount.is_finite() {
                return Err("amount item's structured \"amount\" is not finite");
            }
            let currency = sv
                .get("currency")
                .and_then(Value::as_str)
                .ok_or("amount item's structured_value is missing \"currency\"")?;
            if !is_plausible_currency_code(currency) {
                return Err("amount item's structured \"currency\" is not a 3-letter code");
            }
            Ok(StructuredValue::Amount {
                amount,
                currency: neutralise(currency).to_uppercase(),
            })
        }
        // Reached only for "action_item" (the caller already rejected any
        // other kind) — an action item's structured_value must be null/absent,
        // which the caller checks before calling this function at all.
        _ => Err("action_item must not carry a structured_value"),
    }
}

fn is_plausible_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn is_plausible_currency_code(s: &str) -> bool {
    s.len() == 3 && s.chars().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_well_formed_mixed_response() {
        let raw = r#"[
            {"kind":"action_item","text":"Reply to Jane by Friday"},
            {"kind":"date","text":"Meeting on 25 Sep","structured_value":{"date":"2026-09-25"}},
            {"kind":"amount","text":"Invoice total $1,250.00","structured_value":{"amount":1250.0,"currency":"usd"}}
        ]"#;
        let items = parse_and_validate(raw).expect("valid response");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "action_item");
        assert!(items[0].structured_value.is_none());
        match &items[2].structured_value {
            Some(StructuredValue::Amount { amount, currency }) => {
                assert_eq!(*amount, 1250.0);
                assert_eq!(currency, "USD", "currency is upper-cased");
            }
            other => panic!("expected Amount, got {other:?}"),
        }
    }

    #[test]
    fn accepts_an_empty_array() {
        // A thread with nothing to extract is a legitimate, common result —
        // not a rejection.
        assert_eq!(parse_and_validate("[]").unwrap(), vec![]);
    }

    #[test]
    fn rejects_malformed_json_without_panicking() {
        for bad in [
            "not json at all",
            "{\"kind\":\"action_item\"}",     // object, not array
            "[",                              // truncated
            "[{\"kind\":\"action_item\"}]",   // missing text
            "[{\"kind\":\"summary\",\"text\":\"x\"}]", // unknown kind
            "[{\"kind\":\"date\",\"text\":\"x\",\"structured_value\":{\"date\":\"25/09/2026\"}}]",
            "[{\"kind\":\"amount\",\"text\":\"x\",\"structured_value\":{\"amount\":\"lots\",\"currency\":\"USD\"}}]",
            "[{\"kind\":\"action_item\",\"text\":\"\"}]", // empty text
            "[\"just a string\"]",
        ] {
            assert!(
                parse_and_validate(bad).is_err(),
                "expected rejection for: {bad}"
            );
        }
    }

    #[test]
    fn rejects_too_many_items() {
        let items: Vec<String> = (0..MAX_ITEMS + 1)
            .map(|i| format!(r#"{{"kind":"action_item","text":"item {i}"}}"#))
            .collect();
        let raw = format!("[{}]", items.join(","));
        assert_eq!(parse_and_validate(&raw), Err("response has too many items"));
    }

    #[test]
    fn neutralises_invisible_characters_in_text_and_structured_fields() {
        let raw = "[{\"kind\":\"action_item\",\"text\":\"SEEN\u{200B}HIDDEN\"}]";
        let items = parse_and_validate(raw).unwrap();
        assert_eq!(items[0].text, "SEENHIDDEN");
    }

    #[test]
    fn action_item_rejects_a_structured_value() {
        let raw = r#"[{"kind":"action_item","text":"x","structured_value":{"date":"2026-09-25"}}]"#;
        assert!(parse_and_validate(raw).is_err());
    }
}
