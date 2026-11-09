// Castellum - Ollama NDJSON streaming parser.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure parser for Ollama's streaming chat API response format.
// Each line is a JSON object; we extract the content delta and done flag.
// No prompt text, message subject, or body excerpt ever appears in errors.

use castellum_providers::AiError;
use serde::Deserialize;

/// A parsed line from the Ollama streaming response.
#[derive(Debug, PartialEq)]
pub enum ChatChunk {
    /// A text delta from the assistant turn.
    Delta {
        text: String,
        /// Whether this is the final chunk (`done: true`).
        done: bool,
        /// Tokens consumed in prompt (only present on the done chunk).
        prompt_eval_count: Option<i64>,
        /// Tokens generated (only present on the done chunk).
        eval_count: Option<i64>,
    },
}

/// Raw serde shape for the Ollama `/api/chat` response line.
#[derive(Deserialize)]
struct RawChunk {
    #[serde(default)]
    done: bool,
    #[serde(default)]
    message: Option<RawMessage>,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
    #[serde(default)]
    eval_count: Option<i64>,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: String,
}

/// Structural description of a serde_json error: category and column only.
/// serde's Display renders the offending VALUE for type mismatches (e.g.
/// `invalid type: string "secret", expected a boolean`), which would copy model
/// or prompt text into the stored `ai_jobs.error_message` (#3388).
pub fn describe_json_error(what: &str, e: &serde_json::Error) -> String {
    format!("{what} ({:?} at column {})", e.classify(), e.column())
}

/// Parse one line from the Ollama streaming response.
///
/// Returns `Err(AiError::Backend(...))` for malformed JSON. The error
/// carries only structural information (e.g. the serde message), never
/// the raw line content (which could contain message text).
pub fn parse_chat_line(line: &str) -> Result<ChatChunk, AiError> {
    let raw: RawChunk = serde_json::from_str(line).map_err(|e| {
        AiError::Backend(describe_json_error("invalid JSON from Ollama stream", &e))
    })?;

    let text = raw.message.map(|m| m.content).unwrap_or_default();
    Ok(ChatChunk::Delta {
        text,
        done: raw.done,
        prompt_eval_count: raw.prompt_eval_count,
        eval_count: raw.eval_count,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_parses_delta_chunk() {
        let line = r#"{"model":"llama3.1:8b","created_at":"2026-07-03T00:00:00Z","message":{"role":"assistant","content":"Hello"},"done":false}"#;
        let chunk = parse_chat_line(line).unwrap();
        assert_eq!(
            chunk,
            ChatChunk::Delta {
                text: "Hello".into(),
                done: false,
                prompt_eval_count: None,
                eval_count: None,
            }
        );
    }

    #[test]
    fn ndjson_parses_done_chunk() {
        let line = r#"{"model":"llama3.1:8b","created_at":"2026-07-03T00:00:00Z","message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":42,"eval_count":10}"#;
        let chunk = parse_chat_line(line).unwrap();
        assert_eq!(
            chunk,
            ChatChunk::Delta {
                text: "".into(),
                done: true,
                prompt_eval_count: Some(42),
                eval_count: Some(10),
            }
        );
    }

    #[test]
    fn ndjson_bad_line_is_backend_error() {
        let result = parse_chat_line("not json at all {{{");
        assert!(matches!(result, Err(AiError::Backend(_))));
        // Verify the raw line text doesn't appear in the error.
        if let Err(AiError::Backend(msg)) = result {
            assert!(
                !msg.contains("not json at all"),
                "raw line leaked into error"
            );
        }
    }

    #[test]
    fn ndjson_type_mismatch_error_never_echoes_the_value() {
        // #3388: serde's Display quotes the offending string value.
        for line in [
            r#"{"done":"secret-text"}"#,
            r#"{"message":{"content":["secret-text"]}}"#,
            r#"{"eval_count":"secret-text"}"#,
        ] {
            match parse_chat_line(line) {
                Err(AiError::Backend(msg)) => {
                    assert!(!msg.contains("secret-text"), "value leaked: {msg}")
                }
                other => panic!("expected Backend error, got {other:?}"),
            }
        }
    }

    #[test]
    fn ndjson_missing_message_field_yields_empty_delta() {
        // Ollama sometimes sends done:true with no message field.
        let line = r#"{"done":true,"prompt_eval_count":5,"eval_count":3}"#;
        let chunk = parse_chat_line(line).unwrap();
        assert_eq!(
            chunk,
            ChatChunk::Delta {
                text: "".into(),
                done: true,
                prompt_eval_count: Some(5),
                eval_count: Some(3),
            }
        );
    }
}
