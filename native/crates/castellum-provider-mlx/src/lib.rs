// Castellum - MLX AI provider.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Implements `AiBackend` for MLX (Apple Silicon only).
//
// Verification (2026-09-22):
// - mlx-rs version 0.32.0 (-> mlx-sys 0.6.0, MLX 0.32.2; crates.io:
//   https://crates.io/crates/mlx-rs). Bumped from the originally-scaffolded
//   0.25.3, whose pinned mlx-sys 0.2.0 fails to build on this machine's
//   Xcode 27 / Metal 27.1 toolchain (vendored fence.metal references
//   `metal::thread_scope_system`, absent from that Metal namespace). 0.32
//   builds and runs cleanly here — verified end-to-end (build + a real
//   matmul execution).
// - Maintained by oxiglade (GitHub: https://github.com/oxiglade/mlx-rs)
// - Documentation: https://docs.rs/crate/mlx-rs/latest
// - License: MIT + Apache-2.0 (compatible with workspace Apache-2.0)
// - API surface: model loading from local paths, forward pass inference,
//   generation loop (mlx-lm examples, tranche 3-4 PRs in Sept 2026)
// - Status: active development, maintained, suitable for Option 1 shape
//
// Privacy invariants:
//   * Error messages never contain prompt text, mail content, or model data.
//   * Model files are loaded from app-specific directories, never auto-downloaded.
//   * The defence choke point (S50 defence::envelope) applies identically to
//     this backend via the shared `run::run_ai_jobs_with_backend_at` loop.

use castellum_providers::{
    AiBackend, AiError, GenerateRequest, GenerateSummary, ModelInfo, StreamControl,
};

pub mod embed;

/// MLX AI backend (Apple Silicon + Metal only).
///
/// On macOS aarch64, implements the full `AiBackend` trait (list_models, embed, generate).
/// On all other platforms (Linux runners, Intel macOS, etc.), returns a clear error
/// from every method without attempting to load MLX libraries.
pub struct MlxClient;

impl MlxClient {
    /// Create a new MLX client.
    pub fn new() -> Self {
        MlxClient
    }
}

impl Default for MlxClient {
    fn default() -> Self {
        Self::new()
    }
}

// Platform-gated implementations:
// - Real MLX logic on macOS aarch64 (Task 2-4)
// - Stub returning clear error on all other platforms (Assumption 5)

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod real {
    use super::*;
    use crate::embed::embed_text;

    impl AiBackend for MlxClient {
        fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
            // Task 3: static list of available models
            // MLX has no server to query, so return a hardcoded list of models this stage ships.
            // This matches the model selected in Task 2.
            Ok(vec![ModelInfo {
                // Note: embedding-only in this stage; generation support is Task 4 (stretch).
                name: "all-MiniLM-L6-v2-4bit".to_string(),
                size_bytes: None,
            }])
        }

        fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, AiError> {
            // Task 2: forward pass embedding via mlx-rs.
            embed_text(text, model)
        }

        fn generate(
            &self,
            _req: &GenerateRequest,
            _on_delta: &mut dyn FnMut(&str) -> StreamControl,
        ) -> Result<GenerateSummary, AiError> {
            // Task 4 (stretch): generation loop with streaming
            // Not yet implemented in S56-impl; see Task 4 in the plan.
            Err(AiError::Backend(
                "MLX generate() not yet implemented — see S56-impl Task 4".into(),
            ))
        }
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod stub {
    use super::*;
    use crate::embed::embed_text;

    impl AiBackend for MlxClient {
        fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
            Err(AiError::Backend(
                "MLX is only available on Apple Silicon (macOS aarch64)".into(),
            ))
        }

        fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, AiError> {
            embed_text(text, model)
        }

        fn generate(
            &self,
            _req: &GenerateRequest,
            _on_delta: &mut dyn FnMut(&str) -> StreamControl,
        ) -> Result<GenerateSummary, AiError> {
            Err(AiError::Backend(
                "MLX is only available on Apple Silicon (macOS aarch64)".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_backend_returns_clear_error() {
        // Only used inside the cfg'd-out block below on non-Apple-Silicon
        // targets; on macOS aarch64 (this backend's real platform) the
        // client is constructed but the assertions never run.
        #[allow(unused_variables)]
        let client = MlxClient::new();

        // All three methods should return the same clear error on non-Apple-Silicon.
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            assert!(client.list_models().is_err());
            assert!(client.embed("test", "test").is_err());

            let mut no_op = |_: &str| StreamControl::Continue;
            let req = GenerateRequest {
                model: "test".into(),
                system: "".into(),
                user: "test".into(),
                num_ctx: None,
                max_output_tokens: None,
            };
            assert!(client.generate(&req, &mut no_op).is_err());
        }
    }

    #[test]
    fn mlx_backend_is_aibackend() {
        // Compile-time proof that MlxClient implements AiBackend.
        // This verifies the backend is wire-compatible with the S50 defence
        // choke point (defense::envelope/validate_output) which operates on
        // &mut dyn AiBackend — the MLX backend can be plugged in identically
        // to the Ollama backend.
        let client = MlxClient::new();
        let _backend: &dyn AiBackend = &client;

        // This test doesn't need to run live code — it just proves type
        // compatibility at compile time.
    }
}
