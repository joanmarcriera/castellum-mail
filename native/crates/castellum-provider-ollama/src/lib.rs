// Castellum - Ollama AI provider.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Implements `AiBackend` for a local Ollama instance. Privacy invariants:
//   * The egress guard (`egress::validate_endpoint`) verifies the endpoint is
//     private before any HTTP request is made.
//   * All request URLs are built from the validated IP literal, eliminating
//     DNS re-resolution (connection pinning).
//   * Error messages never contain prompt text or body content.
//
// v1: http only. HTTPS endpoints are deferred until a later slice.

pub mod egress;
pub mod ndjson;

use crate::egress::{validate_endpoint, ValidatedEndpoint};
use crate::ndjson::{describe_json_error, parse_chat_line, ChatChunk};
use castellum_providers::{
    AiBackend, AiError, GenerateRequest, GenerateSummary, ModelInfo, StreamControl,
};
use serde::Deserialize;
use std::fmt;
use std::io::{BufRead, Read};
use std::net::IpAddr;
use std::time::Duration;

/// Longest NDJSON line accepted from the chat stream (1 MiB). Real lines are
/// a few hundred bytes; the cap only stops a hostile/broken endpoint.
const MAX_STREAM_LINE_BYTES: usize = 1024 * 1024;

/// Type alias for the injectable DNS resolver function used in tests.
type ResolverFn<'a> = &'a dyn Fn(&str) -> std::io::Result<Vec<IpAddr>>;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Connection parameters for a local Ollama endpoint.
///
/// `Debug` is implemented manually. Although the config carries no password,
/// the endpoint URL is treated as configuration metadata — only the host/port
/// are printed, keeping the same discipline as `ImapConfig`/`SmtpConfig`.
#[derive(Clone)]
pub struct AiConfig {
    /// Base URL of the Ollama HTTP API (e.g. `"http://127.0.0.1:11434"`).
    /// Must use the `http://` scheme; only private/loopback addresses are
    /// accepted (enforced by `egress::validate_endpoint`).
    pub base_url: String,
    /// Total timeout for a streaming request (wall-clock). Covers the full
    /// stream, not individual reads; generous to allow long generations.
    pub request_timeout_secs: u64,
    /// TCP connect timeout.
    pub connect_timeout_secs: u64,
}

impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("base_url", &self.base_url)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .finish()
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434".into(),
            request_timeout_secs: 300,
            connect_timeout_secs: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A validated, connection-pinned Ollama client.
///
/// The constructor runs `validate_endpoint` eagerly — an `EndpointRefused`
/// error surfaces before any request is attempted. All subsequent requests
/// use the pinned URL (validated IP literal), so DNS is never re-queried.
pub struct OllamaClient {
    endpoint: ValidatedEndpoint,
    agent: ureq::Agent,
}

impl OllamaClient {
    /// Create a client for `config`, validating and pinning the endpoint.
    ///
    /// `resolve` is injected for testing; production callers pass `None`
    /// to use the system resolver.
    pub fn connect(config: &AiConfig) -> Result<Self, AiError> {
        Self::connect_with_resolver(config, None)
    }

    /// Constructor with an injectable resolver (for tests / egress validation).
    pub fn connect_with_resolver(
        config: &AiConfig,
        resolve: Option<ResolverFn<'_>>,
    ) -> Result<Self, AiError> {
        let endpoint = if let Some(r) = resolve {
            validate_endpoint(&config.base_url, r)?
        } else {
            // Production path: system DNS.
            validate_endpoint(&config.base_url, &|host| {
                use std::net::ToSocketAddrs;
                let addrs: Vec<IpAddr> =
                    (host, 0u16).to_socket_addrs()?.map(|sa| sa.ip()).collect();
                Ok(addrs)
            })?
        };

        // Build a ureq agent. ureq does not expose a public `Resolver`
        // trait for custom DNS injection; instead we pin the URL at the
        // validated IP literal (built by `validate_endpoint`), so DNS is
        // bypassed for all requests on this client.
        //
        // redirects(0): ureq follows up to 5 redirects by default. A 3xx from
        // the LAN endpoint would re-send the request — prompt content included
        // — to an arbitrary, unvalidated (possibly public) Location. Redirects
        // are therefore disabled entirely; a 3xx surfaces as an error.
        //
        // ureq 3: `max_redirects(0)` hands a 3xx back as a normal response
        // (`max_redirects_will_error` only applies when redirects are on);
        // 4xx/5xx still surface as `Error::StatusCode` (the default).
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(config.connect_timeout_secs)))
            .timeout_global(Some(Duration::from_secs(config.request_timeout_secs)))
            .max_redirects(0)
            // ureq 3 reads ALL_PROXY/HTTPS_PROXY/HTTP_PROXY from the
            // environment by default (ureq 2 did not). Never route Ollama
            // traffic via a proxy: it would bypass the validated, pinned
            // IP-literal egress policy.
            .proxy(None)
            .build()
            .into();

        Ok(Self { endpoint, agent })
    }

    /// Build a URL for an API path, using the pinned IP base URL.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.endpoint.pinned_url, path)
    }
}

// ---------------------------------------------------------------------------
// Wire types for Ollama /api/tags (list models)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
    size: Option<i64>,
}

// ---------------------------------------------------------------------------
// Wire types for Ollama /api/chat (generate)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<ChatOptions>,
}

#[derive(serde::Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(serde::Serialize)]
struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

// ---------------------------------------------------------------------------
// Wire types for Ollama /api/embeddings (S52)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    embedding: Vec<f32>,
}

// ---------------------------------------------------------------------------
// AiBackend impl
// ---------------------------------------------------------------------------

impl AiBackend for OllamaClient {
    fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let url = self.url("/api/tags");
        let resp = self.agent.get(&url).call().map_err(map_ureq_error)?;
        reject_non_success(&resp)?;

        let body = read_body_string(resp)
            .map_err(|e| AiError::Backend(format!("failed to read /api/tags response: {e}")))?;
        let tags: TagsResponse = serde_json::from_str(&body).map_err(|e| {
            AiError::Backend(describe_json_error(
                "failed to parse /api/tags response",
                &e,
            ))
        })?;

        Ok(tags
            .models
            .into_iter()
            .map(|m| ModelInfo {
                name: m.name,
                size_bytes: m.size,
            })
            .collect())
    }

    fn generate(
        &self,
        req: &GenerateRequest,
        on_delta: &mut dyn FnMut(&str) -> StreamControl,
    ) -> Result<GenerateSummary, AiError> {
        let url = self.url("/api/chat");

        let options = if req.num_ctx.is_some() || req.max_output_tokens.is_some() {
            Some(ChatOptions {
                num_ctx: req.num_ctx,
                num_predict: req.max_output_tokens,
            })
        } else {
            None
        };

        let body = ChatRequest {
            model: &req.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: &req.system,
                },
                ChatMessage {
                    role: "user",
                    content: &req.user,
                },
            ],
            stream: true,
            options,
        };

        let body_str = serde_json::to_string(&body)
            .map_err(|e| AiError::Backend(format!("failed to serialize request: {e}")))?;

        let resp = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body_str.as_str())
            .map_err(|e| {
                if matches!(&e, ureq::Error::StatusCode(404)) {
                    return AiError::ModelMissing(req.model.clone());
                }
                map_ureq_error(e)
            })?;
        reject_non_success(&resp)?;

        // Stream the NDJSON response line by line.
        let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
        let mut buf: Vec<u8> = Vec::new();
        let mut summary = GenerateSummary::default();

        loop {
            buf.clear();
            // Bounded read: a newline-free stream must not grow memory
            // without limit (#3388). `take` is re-created per line.
            let n = (&mut reader)
                .take(MAX_STREAM_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut buf)
                .map_err(|e| {
                    // A read timeout surfaces as a TimedOut io::Error.
                    if e.kind() == std::io::ErrorKind::TimedOut {
                        AiError::Timeout
                    } else {
                        AiError::Network(format!("read error on stream: {e}"))
                    }
                })?;

            if n == 0 {
                // EOF without a done:true chunk.
                break;
            }
            if buf.len() > MAX_STREAM_LINE_BYTES {
                return Err(AiError::Backend(
                    "Ollama stream line exceeds size limit".into(),
                ));
            }
            let line = std::str::from_utf8(&buf)
                .map_err(|_| AiError::Backend("Ollama stream line is not valid UTF-8".into()))?;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let chunk = parse_chat_line(trimmed)?;
            match chunk {
                ChatChunk::Delta {
                    text,
                    done,
                    prompt_eval_count,
                    eval_count,
                } => {
                    if !text.is_empty() {
                        let ctrl = on_delta(&text);
                        if ctrl == StreamControl::Cancel {
                            // Drop the reader to close the socket, aborting Ollama-side.
                            drop(reader);
                            return Err(AiError::Cancelled);
                        }
                    }
                    if done {
                        summary.tokens_in = prompt_eval_count;
                        summary.tokens_out = eval_count;
                        return Ok(summary);
                    }
                }
            }
        }

        // Reached EOF without a done chunk — treat as a backend error.
        Err(AiError::Backend(
            "stream ended without done:true sentinel".into(),
        ))
    }

    fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, AiError> {
        let url = self.url("/api/embeddings");

        let body = EmbeddingsRequest {
            model,
            prompt: text,
        };
        let body_str = serde_json::to_string(&body).map_err(|e| {
            AiError::Backend(format!("failed to serialize embeddings request: {e}"))
        })?;

        let resp = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body_str.as_str())
            .map_err(|e| {
                if matches!(&e, ureq::Error::StatusCode(404)) {
                    return AiError::ModelMissing(model.to_string());
                }
                map_ureq_error(e)
            })?;
        reject_non_success(&resp)?;

        let body = read_body_string(resp).map_err(|e| {
            AiError::Backend(format!("failed to read /api/embeddings response: {e}"))
        })?;
        let parsed: EmbeddingsResponse = serde_json::from_str(&body).map_err(|e| {
            AiError::Backend(describe_json_error(
                "failed to parse /api/embeddings response",
                &e,
            ))
        })?;

        if parsed.embedding.is_empty() {
            return Err(AiError::Backend(
                "Ollama returned an empty embedding vector".into(),
            ));
        }

        // inf/NaN would be stored and poison every similarity ranking (#3377).
        if parsed.embedding.iter().any(|x| !x.is_finite()) {
            return Err(AiError::Backend(
                "Ollama returned a non-finite embedding value".into(),
            ));
        }

        Ok(parsed.embedding)
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Refuse any non-2xx response that ureq returned as `Ok`.
///
/// With `redirects(0)` a 3xx is handed back as a normal response rather than
/// being followed. Following it would re-send the request — prompt included —
/// to an unvalidated Location, so any non-success status is a hard error here.
/// The error carries only the numeric status (never body or header content).
fn reject_non_success(resp: &ureq::http::Response<ureq::Body>) -> Result<(), AiError> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    Err(AiError::Backend(format!(
        "unexpected HTTP {status} from Ollama (redirects are not followed)"
    )))
}

/// Read a response body to a `String`. `Body::read_to_string` keeps ureq 2's
/// `into_string` 10 MB cap, bounding memory against a misbehaving endpoint.
fn read_body_string(resp: ureq::http::Response<ureq::Body>) -> Result<String, ureq::Error> {
    resp.into_body().read_to_string()
}

/// Map a ureq 3 error onto `AiError` by variant / `io::ErrorKind`, not by
/// message text.
fn map_ureq_error(e: ureq::Error) -> AiError {
    use std::io::ErrorKind;
    match e {
        ureq::Error::StatusCode(404) => {
            AiError::ModelMissing("unknown (404 from /api/chat)".into())
        }
        ureq::Error::StatusCode(code) => AiError::Backend(format!("HTTP {code} from Ollama")),
        ureq::Error::Timeout(_) => AiError::Timeout,
        ureq::Error::Io(io) => match io.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => AiError::Timeout,
            ErrorKind::ConnectionRefused => AiError::Network(format!("connection refused: {io}")),
            _ => AiError::Network(io.to_string()),
        },
        other => AiError::Network(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Integration tests (loopback TcpListener)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Spin up a one-shot HTTP server on a random loopback port that serves
    /// a canned response, then let the test function drive the client.
    ///
    /// The server thread keeps the TCP connection **open** until `f` returns,
    /// then closes it. This prevents a race on macOS where the OS can return
    /// EINVAL if the server-side FIN arrives while ureq is still parsing
    /// response headers (observed when tests run concurrently).
    fn with_mock_server<F>(response: &'static str, f: F)
    where
        F: FnOnce(u16),
    {
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request (one read is sufficient for small test bodies).
                let mut req_buf = [0u8; 4096];
                let _ = stream.read(&mut req_buf);
                let _ = stream.write_all(response.as_bytes());
                // Hold the connection open until the test signals done so that
                // ureq can finish reading headers/body without racing the FIN.
                let _ = done_rx.recv();
            }
        });

        f(port);
        drop(done_tx); // signal server to close
        let _ = handle.join();
    }

    fn ollama_config(port: u16) -> AiConfig {
        AiConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            request_timeout_secs: 5,
            connect_timeout_secs: 2,
        }
    }

    #[test]
    fn env_proxy_is_ignored_and_request_goes_direct() {
        // A dead proxy in the environment must not capture the request: the
        // client has to reach the mock directly (egress policy). Only this
        // test touches proxy vars, and it restores them.
        let body = "{\"models\":[]}";
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let vars = [
            "ALL_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "all_proxy",
            "http_proxy",
            "https_proxy",
        ];
        for v in vars {
            std::env::set_var(v, "http://127.0.0.1:1");
        }
        with_mock_server(response, |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let models = client.list_models();
            for v in vars {
                std::env::remove_var(v);
            }
            assert!(models.expect("direct request must succeed").is_empty());
        });
        for v in vars {
            std::env::remove_var(v);
        }
    }

    #[test]
    fn generate_streams_deltas_and_summary() {
        // Canned Ollama streaming response: two delta chunks + done chunk.
        let ndjson_body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\" world\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":10,\"eval_count\":5}\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{len:x}\r\n{ndjson_body}\r\n0\r\n\r\n",
            len = ndjson_body.len(),
        );

        with_mock_server(Box::leak(response.into_boxed_str()), |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let req = GenerateRequest {
                model: "llama3.1:8b".into(),
                system: "You are a helpful assistant.".into(),
                user: "Summarise: test".into(),
                num_ctx: None,
                max_output_tokens: None,
            };
            let mut deltas: Vec<String> = Vec::new();
            let result = client.generate(&req, &mut |text| {
                deltas.push(text.to_string());
                StreamControl::Continue
            });
            let summary = result.expect("generation should succeed");
            assert_eq!(deltas, vec!["Hello", " world"]);
            assert_eq!(summary.tokens_in, Some(10));
            assert_eq!(summary.tokens_out, Some(5));
        });
    }

    #[test]
    fn generate_cancel_midstream_stops_reading() {
        let ndjson_body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"chunk1\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"chunk2\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":10,\"eval_count\":5}\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{len:x}\r\n{ndjson_body}\r\n0\r\n\r\n",
            len = ndjson_body.len(),
        );

        with_mock_server(Box::leak(response.into_boxed_str()), |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let req = GenerateRequest {
                model: "llama3.1:8b".into(),
                system: "sys".into(),
                user: "user".into(),
                num_ctx: None,
                max_output_tokens: None,
            };
            let mut count = 0u32;
            let result = client.generate(&req, &mut |_text| {
                count += 1;
                // Cancel after the first delta.
                StreamControl::Cancel
            });
            assert!(
                matches!(result, Err(AiError::Cancelled)),
                "expected Cancelled, got {:?}",
                result
            );
            assert_eq!(count, 1, "callback invoked exactly once before cancel");
        });
    }

    #[test]
    fn generate_refuses_redirect_and_never_follows() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_srv = Arc::clone(&requests);

        // The Location points back at THIS mock server (different path): if the
        // client followed redirects, the mock would observably receive a second
        // request. (A public Location like http://8.8.8.8/ could not be
        // observed from the test.) Following any redirect would equally re-send
        // the prompt to an unvalidated address — that is what must not happen.
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/elsewhere\r\nContent-Length: 0\r\n\r\n",
        );

        /// Read one complete HTTP request (headers + Content-Length body),
        /// which may arrive across several TCP segments. Returns false on
        /// EOF/timeout before any bytes (the sentinel unblock connection).
        fn read_full_request(stream: &mut std::net::TcpStream) -> bool {
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => return !buf.is_empty(),
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
                let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                let content_length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut remaining = content_length.saturating_sub(buf.len() - (pos + 4));
                while remaining > 0 {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                    }
                }
                return true;
            }
        }

        let handle = thread::spawn(move || {
            // Serve up to 2 connections so a redirect-following client would be
            // seen. A request-less connection = the test's sentinel unblock.
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                if !read_full_request(&mut stream) {
                    break; // sentinel connection — test is done
                }
                requests_srv.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(response.as_bytes());
                // Catch a keep-alive reuse of the same connection for the
                // redirect target (would not show up as a new accept).
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                if read_full_request(&mut stream) {
                    requests_srv.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let client = OllamaClient::connect(&ollama_config(port)).unwrap();
        const SENTINEL: &str = "REDIRECT_LEAK_SENTINEL_9c41";
        let req = GenerateRequest {
            model: "llama3.1:8b".into(),
            system: format!("system {SENTINEL}"),
            user: format!("user {SENTINEL}"),
            num_ctx: None,
            max_output_tokens: None,
        };
        let result = client.generate(&req, &mut |_| StreamControl::Continue);
        assert!(
            matches!(result, Err(AiError::Backend(_)) | Err(AiError::Network(_))),
            "redirect must surface as an error, got {result:?}"
        );
        let err_dbg = format!("{:?}", result.unwrap_err());
        assert!(
            !err_dbg.contains(SENTINEL),
            "prompt content leaked into redirect error: {err_dbg}"
        );

        // Unblock the server's second accept, then verify exactly ONE request
        // reached the mock — the redirect was never followed.
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        handle.join().unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "client must never issue a second request after a redirect"
        );
    }

    #[test]
    fn embed_parses_vector_from_ollama_response() {
        let body = r#"{"embedding":[0.1,-0.25,0.5]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        );

        with_mock_server(Box::leak(response.into_boxed_str()), |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let vector = client
                .embed("nomic-embed-text", "Subject: hi\n\nBody text.")
                .expect("embed should succeed");
            assert_eq!(vector, vec![0.1, -0.25, 0.5]);
        });
    }

    #[test]
    fn generate_rejects_an_overlong_stream_line() {
        // #3388: ~2 MiB with no newline must fail fast, not buffer forever.
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{}",
                "x".repeat(2 * 1024 * 1024)
            )
            .into_boxed_str(),
        );
        with_mock_server(response, |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let req = GenerateRequest {
                model: "m".into(),
                system: "s".into(),
                user: "u".into(),
                num_ctx: None,
                max_output_tokens: None,
            };
            let result = client.generate(&req, &mut |_| StreamControl::Continue);
            match result {
                Err(AiError::Backend(msg)) => assert!(msg.contains("size limit"), "{msg}"),
                other => panic!("expected size-limit error, got {other:?}"),
            }
        });
    }

    #[test]
    fn embed_rejects_out_of_f32_range_values() {
        // #3377: 1e39 narrows to inf (or errors); either way nothing is stored.
        let body = r#"{"embedding":[1e39,0.1]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        with_mock_server(Box::leak(response.into_boxed_str()), |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            assert!(
                matches!(client.embed("m", "t"), Err(AiError::Backend(_))),
                "an out-of-range embedding must be rejected"
            );
        });
    }

    #[test]
    fn embed_404_maps_to_model_missing() {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";

        with_mock_server(response, |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let result = client.embed("does-not-exist", "text");
            assert!(
                matches!(result, Err(AiError::ModelMissing(_))),
                "expected ModelMissing, got {:?}",
                result
            );
        });
    }

    #[test]
    fn embed_never_follows_redirect_and_never_leaks_text_in_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/elsewhere\r\nContent-Length: 0\r\n\r\n",
        );

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let client = OllamaClient::connect(&ollama_config(port)).unwrap();
        const SENTINEL: &str = "EMBED_REDIRECT_LEAK_SENTINEL_4f1a";
        let result = client.embed("nomic-embed-text", SENTINEL);
        assert!(
            matches!(result, Err(AiError::Backend(_)) | Err(AiError::Network(_))),
            "redirect must surface as an error, got {result:?}"
        );
        let err_dbg = format!("{:?}", result.unwrap_err());
        assert!(
            !err_dbg.contains(SENTINEL),
            "message text leaked into redirect error: {err_dbg}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn generate_404_maps_to_model_missing() {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";

        with_mock_server(response, |port| {
            let client = OllamaClient::connect(&ollama_config(port)).unwrap();
            let req = GenerateRequest {
                model: "does-not-exist".into(),
                system: "sys".into(),
                user: "user".into(),
                num_ctx: None,
                max_output_tokens: None,
            };
            let result = client.generate(&req, &mut |_| StreamControl::Continue);
            assert!(
                matches!(result, Err(AiError::ModelMissing(_))),
                "expected ModelMissing, got {:?}",
                result
            );
        });
    }

    // ---- error-kind mapping (ureq 3 variants -> AiError) ----

    #[test]
    fn map_status_errors() {
        assert!(matches!(
            map_ureq_error(ureq::Error::StatusCode(404)),
            AiError::ModelMissing(_)
        ));
        assert!(
            matches!(map_ureq_error(ureq::Error::StatusCode(500)), AiError::Backend(m) if m.contains("500"))
        );
    }

    #[test]
    fn map_timeout_variants() {
        assert!(matches!(
            map_ureq_error(ureq::Error::Timeout(ureq::Timeout::Global)),
            AiError::Timeout
        ));
        let io = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(matches!(
            map_ureq_error(ureq::Error::Io(io)),
            AiError::Timeout
        ));
    }

    #[test]
    fn map_io_kinds_to_network() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            map_ureq_error(ureq::Error::Io(refused)),
            AiError::Network(m) if m.starts_with("connection refused")
        ));
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(matches!(
            map_ureq_error(ureq::Error::Io(reset)),
            AiError::Network(_)
        ));
    }

    #[test]
    fn connection_refused_end_to_end_is_network_error() {
        // Bind then drop to obtain a loopback port nobody listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = OllamaClient::connect(&ollama_config(port)).unwrap();
        let result = client.list_models();
        assert!(
            matches!(result, Err(AiError::Network(_))),
            "expected Network, got {result:?}"
        );
    }
}
