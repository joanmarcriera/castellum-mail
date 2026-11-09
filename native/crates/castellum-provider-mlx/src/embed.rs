// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Embedding implementation for MLX backend (Apple Silicon only).
//
// Selected model: mlx-community/all-MiniLM-L6-v2-4bit
// Source: https://huggingface.co/mlx-community/all-MiniLM-L6-v2-4bit (accessed 2026-09-25)
// License: Apache-2.0 (from sentence-transformers/all-MiniLM-L6-v2, re-licensed by the
//   mlx-community conversion; confirmed via the HF Hub API model card, 2026-09-25)
// Architecture (model.safetensors.index.json + config.json, accessed 2026-09-25):
//   BERT, 6 encoder layers, hidden_size 384, 12 attention heads (head_dim 32),
//   intermediate_size 1536, vocab_size 30522, layer_norm_eps 1e-12,
//   4-bit group-quantized (group_size 64) weights + a plain float bias per linear layer.
// Tokenizer: the model ships its own `tokenizer.json` (HF "fast tokenizer" format),
//   loaded directly via the `tokenizers` crate — no hand-rolled WordPiece implementation.
//
// This is a small, pre-quantized embedding model suitable for search and semantic matching.
//
// # Where the weights live
//
// MLX has no daemon (unlike Ollama), so this crate is responsible for locating the model
// files itself. They are looked up in, in order:
//   1. `$CASTELLUM_MLX_MODEL_DIR/<model>/` if that env var is set (test/dev override).
//   2. `$HOME/Library/Caches/uk.co.riera.castellum/mlx-models/<model>/` otherwise.
// Two files are required in that directory: `model.safetensors` and `tokenizer.json`
// (both downloadable from the model's Hugging Face repo, see the URL above). Weights are
// never bundled in the repo and never auto-downloaded by this stage (S56-impl) — a missing
// model is reported as `AiError::ModelMissing`, never a panic, never a silently-empty vector.
// Auto-fetch-on-first-use is a deliberately deferred follow-up (see the S56-impl worker report).

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
use crate::AiError;

/// Mean-pool a sequence of per-token vectors (e.g. BERT's last hidden state) into a single
/// fixed-dimension embedding, weighting by an optional attention mask (1.0 = real token,
/// 0.0 = padding). All tokens are treated as real when `mask` is `None`.
///
/// Pure `Vec<f32>` math, no MLX/Array dependency — testable on every platform without any
/// model weights.
// Only the Apple-Silicon forward pass calls these; the stub build keeps them for the
// weight-free unit tests, so they are dead code there outside `cfg(test)`.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
pub(crate) fn mean_pool(tokens: &[Vec<f32>], mask: Option<&[f32]>) -> Vec<f32> {
    let dim = tokens.first().map(|t| t.len()).unwrap_or(0);
    let mut sum = vec![0.0f32; dim];
    let mut weight_total = 0.0f32;
    for (i, token) in tokens.iter().enumerate() {
        let weight = mask.map(|m| m[i]).unwrap_or(1.0);
        if weight == 0.0 {
            continue;
        }
        for (s, v) in sum.iter_mut().zip(token.iter()) {
            *s += v * weight;
        }
        weight_total += weight;
    }
    if weight_total > 0.0 {
        for s in sum.iter_mut() {
            *s /= weight_total;
        }
    }
    sum
}

/// L2-normalise a vector in place. A zero (or near-zero) vector is left unchanged rather
/// than dividing by zero.
///
/// Pure `Vec<f32>` math — testable on every platform without any model weights.
// Only the Apple-Silicon forward pass calls these; the stub build keeps them for the
// weight-free unit tests, so they are dead code there outside `cfg(test)`.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
pub(crate) fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod real {
    use super::{l2_normalize, mean_pool};
    use crate::AiError;
    use mlx_rs::{
        array,
        error::Exception,
        ops::{
            self, add, divide, matmul, mean_axis, multiply, quantized_matmul, reshape,
            softmax_axis, subtract, swap_axes, var_axis,
        },
        Array,
    };
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Fixed model architecture for `mlx-community/all-MiniLM-L6-v2-4bit` — see the module
    /// doc comment for the source citation. This crate ships exactly one embedding model in
    /// S56-impl, so these are constants rather than a parsed `config.json`; a future stage
    /// adding a second embedding model should read them from disk instead.
    const HIDDEN_SIZE: i32 = 384;
    const NUM_LAYERS: usize = 6;
    const NUM_HEADS: i32 = 12;
    const HEAD_DIM: i32 = HIDDEN_SIZE / NUM_HEADS;
    const LAYER_NORM_EPS: f32 = 1e-12;
    const QUANT_GROUP_SIZE: i32 = 64;
    const QUANT_BITS: i32 = 4;

    fn mlx_err(context: &str, e: Exception) -> AiError {
        // `e` is an MLX/shape-level error; it never contains prompt or mail text.
        AiError::Backend(format!("MLX error in {context}: {e}"))
    }

    /// Resolve the on-disk directory a model's weights + tokenizer are expected in.
    /// See the module doc comment for the lookup order.
    fn model_dir(model: &str) -> PathBuf {
        if let Ok(override_dir) = std::env::var("CASTELLUM_MLX_MODEL_DIR") {
            return PathBuf::from(override_dir).join(model);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        Path::new(&home)
            .join("Library/Caches/uk.co.riera.castellum/mlx-models")
            .join(model)
    }

    /// Ensure the two required files are present, returning their paths.
    /// Never downloads anything — a missing model is a clear, actionable `AiError`.
    pub(super) fn require_model_files(model: &str) -> Result<(PathBuf, PathBuf), AiError> {
        let dir = model_dir(model);
        let weights = dir.join("model.safetensors");
        let tokenizer = dir.join("tokenizer.json");
        if !weights.is_file() || !tokenizer.is_file() {
            return Err(AiError::ModelMissing(format!(
                "{model} (expected model.safetensors + tokenizer.json in {}; \
                 fetch from https://huggingface.co/mlx-community/{model})",
                dir.display()
            )));
        }
        Ok((weights, tokenizer))
    }

    /// Dequantize a 4-bit group-quantized weight/embedding matrix to plain f32.
    fn dequantize(weights: &HashMap<String, Array>, prefix: &str) -> Result<Array, AiError> {
        let w = weights
            .get(&format!("{prefix}.weight"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let scales = weights
            .get(&format!("{prefix}.scales"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let biases = weights
            .get(&format!("{prefix}.biases"))
            .ok_or_else(|| missing_tensor(prefix))?;
        ops::dequantize(w, scales, Some(biases), QUANT_GROUP_SIZE, QUANT_BITS)
            .map_err(|e| mlx_err(&format!("dequantize({prefix})"), e))
    }

    fn missing_tensor(prefix: &str) -> AiError {
        AiError::Backend(format!(
            "MLX model weights are missing an expected tensor under '{prefix}' \
             (unexpected safetensors layout for this model)"
        ))
    }

    /// A 4-bit group-quantized `nn.Linear`: `y = quantized_matmul(x, w) + bias`.
    fn quantized_linear(
        weights: &HashMap<String, Array>,
        prefix: &str,
        x: &Array,
    ) -> Result<Array, AiError> {
        let w = weights
            .get(&format!("{prefix}.weight"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let scales = weights
            .get(&format!("{prefix}.scales"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let biases = weights
            .get(&format!("{prefix}.biases"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let bias = weights
            .get(&format!("{prefix}.bias"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let y = quantized_matmul(
            x,
            w,
            scales,
            Some(biases),
            true,
            QUANT_GROUP_SIZE,
            QUANT_BITS,
        )
        .map_err(|e| mlx_err(&format!("quantized_matmul({prefix})"), e))?;
        add(&y, bias).map_err(|e| mlx_err(&format!("bias add({prefix})"), e))
    }

    /// `LayerNorm` over the last axis: `(x - mean) / sqrt(var + eps) * weight + bias`.
    fn layer_norm(
        weights: &HashMap<String, Array>,
        prefix: &str,
        x: &Array,
    ) -> Result<Array, AiError> {
        let weight = weights
            .get(&format!("{prefix}.weight"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let bias = weights
            .get(&format!("{prefix}.bias"))
            .ok_or_else(|| missing_tensor(prefix))?;
        let ctx = || format!("layer_norm({prefix})");
        let mean = mean_axis(x, -1, true).map_err(|e| mlx_err(&ctx(), e))?;
        let centered = subtract(x, &mean).map_err(|e| mlx_err(&ctx(), e))?;
        let variance = var_axis(x, -1, true, 0).map_err(|e| mlx_err(&ctx(), e))?;
        let denom =
            ops::sqrt(add(&variance, array!(LAYER_NORM_EPS)).map_err(|e| mlx_err(&ctx(), e))?)
                .map_err(|e| mlx_err(&ctx(), e))?;
        let normalized = divide(&centered, &denom).map_err(|e| mlx_err(&ctx(), e))?;
        let scaled = multiply(&normalized, weight).map_err(|e| mlx_err(&ctx(), e))?;
        add(&scaled, bias).map_err(|e| mlx_err(&ctx(), e))
    }

    /// Look up rows of a dequantized embedding table by integer id.
    fn embedding_lookup(table: &Array, ids: &[i32]) -> Result<Array, AiError> {
        let ids_arr = Array::from_slice(ids, &[ids.len() as i32]);
        ops::indexing::take_axis(table, &ids_arr, 0).map_err(|e| mlx_err("embedding_lookup", e))
    }

    /// Run the BERT encoder forward pass, returning the final hidden state, shape
    /// `(seq_len, HIDDEN_SIZE)` (no batch dimension — this crate embeds one text at a time).
    ///
    /// `attention_mask` (length `token_ids.len()`, 1.0 = real token, 0.0 = padding) comes
    /// straight from the tokenizer's `Encoding`. It matters even for a single input: this
    /// model's shipped `tokenizer.json` has `padding.strategy = Fixed(128)`, so a short input
    /// is padded out to 128 tokens before it ever reaches this function. Without masking,
    /// every real token's hidden state gets contaminated by attending to ~120 [PAD]
    /// positions, which is what produced wrong (pad-dominated) similarity scores.
    fn forward(
        weights: &HashMap<String, Array>,
        token_ids: &[i32],
        attention_mask: &[f32],
    ) -> Result<Array, AiError> {
        let seq_len = token_ids.len() as i32;

        let word_table = dequantize(weights, "embeddings.word_embeddings")?;
        let pos_table = dequantize(weights, "embeddings.position_embeddings")?;
        let type_table = dequantize(weights, "embeddings.token_type_embeddings")?;

        let position_ids: Vec<i32> = (0..seq_len).collect();
        let type_ids = vec![0i32; token_ids.len()];

        let word_embeds = embedding_lookup(&word_table, token_ids)?;
        let pos_embeds = embedding_lookup(&pos_table, &position_ids)?;
        let type_embeds = embedding_lookup(&type_table, &type_ids)?;

        let summed = add(&word_embeds, &pos_embeds).map_err(|e| mlx_err("embed sum", e))?;
        let summed = add(&summed, &type_embeds).map_err(|e| mlx_err("embed sum", e))?;
        let mut hidden = layer_norm(weights, "embeddings.LayerNorm", &summed)?;

        // Additive attention-mask bias: 0.0 for real key positions, a large negative
        // number for padding, broadcast over (heads, seq_len, seq_len) so it lands on the
        // key axis (last dim) — every query position is blocked from attending to [PAD]
        // positions. Uses a large finite negative rather than -inf: softmax over an
        // all-finite row can't produce NaN, and at least one key (the [CLS] token) is
        // always real, so no row is ever masked out entirely.
        let mask_bias: Vec<f32> = attention_mask
            .iter()
            .map(|&m| if m > 0.0 { 0.0 } else { -1e9 })
            .collect();
        let mask_arr = Array::from_slice(&mask_bias, &[1, 1, seq_len]);

        for layer in 0..NUM_LAYERS {
            let p = format!("encoder.layer.{layer}");

            let q = quantized_linear(weights, &format!("{p}.attention.self.query"), &hidden)?;
            let k = quantized_linear(weights, &format!("{p}.attention.self.key"), &hidden)?;
            let v = quantized_linear(weights, &format!("{p}.attention.self.value"), &hidden)?;

            // (seq_len, hidden) -> (seq_len, heads, head_dim) -> (heads, seq_len, head_dim)
            let to_heads = |x: &Array| -> Result<Array, AiError> {
                let r = reshape(x, &[seq_len, NUM_HEADS, HEAD_DIM])
                    .map_err(|e| mlx_err("reshape to heads", e))?;
                swap_axes(&r, 0, 1).map_err(|e| mlx_err("swap_axes to heads", e))
            };
            let qh = to_heads(&q)?;
            let kh = to_heads(&k)?;
            let vh = to_heads(&v)?;

            let kh_t = swap_axes(&kh, 1, 2).map_err(|e| mlx_err("transpose K", e))?;
            let scores = matmul(&qh, &kh_t).map_err(|e| mlx_err("Q @ K^T", e))?;
            let scale = 1.0f32 / (HEAD_DIM as f32).sqrt();
            let scaled_scores =
                multiply(&scores, array!(scale)).map_err(|e| mlx_err("scale scores", e))?;
            let masked_scores =
                add(&scaled_scores, &mask_arr).map_err(|e| mlx_err("apply attention mask", e))?;
            let probs =
                softmax_axis(&masked_scores, -1, true).map_err(|e| mlx_err("softmax", e))?;
            let context = matmul(&probs, &vh).map_err(|e| mlx_err("softmax @ V", e))?;

            // (heads, seq_len, head_dim) -> (seq_len, heads, head_dim) -> (seq_len, hidden)
            let context = swap_axes(&context, 0, 1).map_err(|e| mlx_err("swap_axes back", e))?;
            let context = reshape(&context, &[seq_len, HIDDEN_SIZE])
                .map_err(|e| mlx_err("reshape back", e))?;

            let attn_out =
                quantized_linear(weights, &format!("{p}.attention.output.dense"), &context)?;
            let residual1 =
                add(&attn_out, &hidden).map_err(|e| mlx_err("attention residual", e))?;
            let hidden1 = layer_norm(
                weights,
                &format!("{p}.attention.output.LayerNorm"),
                &residual1,
            )?;

            let intermediate =
                quantized_linear(weights, &format!("{p}.intermediate.dense"), &hidden1)?;
            let activated = mlx_rs::nn::gelu(&intermediate).map_err(|e| mlx_err("gelu", e))?;
            let ffn_out = quantized_linear(weights, &format!("{p}.output.dense"), &activated)?;
            let residual2 = add(&ffn_out, &hidden1).map_err(|e| mlx_err("ffn residual", e))?;
            hidden = layer_norm(weights, &format!("{p}.output.LayerNorm"), &residual2)?;
        }

        Ok(hidden)
    }

    /// Embed `text` using the model named `model` (must match one of `list_models()`'s
    /// entries; the weights are located per the module doc comment).
    pub(crate) fn embed_text(text: &str, model: &str) -> Result<Vec<f32>, AiError> {
        let (weights_path, tokenizer_path) = require_model_files(model)?;

        let weights = Array::load_safetensors(&weights_path)
            .map_err(|e| AiError::Backend(format!("failed to load {model} weights: {e}")))?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| AiError::Backend(format!("failed to load {model} tokenizer: {e}")))?;
        let encoding = tokenizer
            .encode(text, true)
            .map_err(|e| AiError::Backend(format!("failed to tokenize input for {model}: {e}")))?;
        let token_ids: Vec<i32> = encoding.get_ids().iter().map(|&id| id as i32).collect();
        if token_ids.is_empty() {
            // Should not happen once special tokens are added, but never panic on it.
            return Ok(vec![0.0; HIDDEN_SIZE as usize]);
        }
        // The tokenizer's own tokenizer.json pads every input to a fixed length (see
        // module doc comment); `attention_mask` is what tells the model, and this
        // function's pooling step, which of those positions are real tokens vs [PAD].
        let attention_mask: Vec<f32> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as f32)
            .collect();

        let hidden = forward(&weights, &token_ids, &attention_mask)?;
        let seq_len = token_ids.len();
        hidden.eval().map_err(|e| mlx_err("eval forward pass", e))?;
        let flat: &[f32] = hidden.as_slice::<f32>();

        let per_token: Vec<Vec<f32>> = (0..seq_len)
            .map(|i| flat[i * HIDDEN_SIZE as usize..(i + 1) * HIDDEN_SIZE as usize].to_vec())
            .collect();

        let mut pooled = mean_pool(&per_token, Some(&attention_mask));
        l2_normalize(&mut pooled);
        Ok(pooled)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use real::embed_text;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn embed_text(_text: &str, _model: &str) -> Result<Vec<f32>, AiError> {
    Err(AiError::Backend(
        "MLX is only available on Apple Silicon (macOS aarch64)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_pool_averages_tokens_without_mask() {
        let tokens = vec![vec![1.0, 2.0, 3.0], vec![3.0, 4.0, 5.0]];
        let pooled = mean_pool(&tokens, None);
        assert_eq!(pooled, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn mean_pool_ignores_masked_out_tokens() {
        let tokens = vec![vec![1.0, 1.0], vec![9.0, 9.0], vec![5.0, 5.0]];
        // Middle token is padding (mask 0.0) and must not affect the result.
        let pooled = mean_pool(&tokens, Some(&[1.0, 0.0, 1.0]));
        assert_eq!(pooled, vec![3.0, 3.0]);
    }

    /// Reproduces the exact shape of the reviewer-caught bug at the `mean_pool` level,
    /// with no model weights involved: `mlx-community/all-MiniLM-L6-v2-4bit`'s
    /// `tokenizer.json` pads every input to a fixed 128 tokens, so a real embed had a
    /// handful of real-token vectors swamped by ~120 padding-token vectors once
    /// `embed_text` mean-pooled with `mask: None` (the old, buggy call path — see the
    /// `Some(&attention_mask)` call in `embed_text` today). This test fails if that old
    /// call path is restored: masked pooling must match "real tokens only", and must
    /// diverge sharply from unmasked pooling once padding dominates the token count.
    #[test]
    fn mean_pool_masked_matches_real_tokens_only_not_padding_dominated() {
        let real_tokens = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]];
        let padding_tokens = vec![vec![50.0, 50.0]; 5]; // outlier values, like a [PAD] embedding
        let all_tokens: Vec<Vec<f32>> = real_tokens.iter().cloned().chain(padding_tokens).collect();
        let mask = [1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];

        let masked = mean_pool(&all_tokens, Some(&mask));
        let unmasked = mean_pool(&all_tokens, None); // the OLD (buggy) `embed_text` call path
        let real_only = mean_pool(&real_tokens, None); // ground truth

        assert_eq!(
            masked, real_only,
            "masking must ignore padding entirely and match the real-tokens-only average"
        );
        assert_ne!(
            masked, unmasked,
            "unmasked pooling (the old bug) must diverge once padding dominates the token count"
        );
    }

    #[test]
    fn mean_pool_empty_input_returns_empty_vector() {
        let pooled = mean_pool(&[], None);
        assert_eq!(pooled, Vec::<f32>::new());
    }

    #[test]
    fn l2_normalize_produces_unit_vector() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "expected unit norm, got {norm}");
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_leaves_zero_vector_unchanged() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    /// Returns `Some(model_id)` iff real MLX model weights are present on disk for the
    /// hardware-gated tests below, logging why they are being skipped otherwise. Keeps the
    /// tests green (not merely `#[ignore]`d-and-forgotten) on any Apple-Silicon runner that
    /// simply hasn't had the model files placed in its cache directory yet.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn require_test_model() -> Option<&'static str> {
        let model = "all-MiniLM-L6-v2-4bit";
        match real::require_model_files(model) {
            Ok(_) => Some(model),
            Err(e) => {
                eprintln!("skipping MLX hardware test: {e}");
                None
            }
        }
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[ignore = "requires MLX model files on disk (see module doc comment for the expected path)"]
    fn test_embed_determinism() {
        let Some(model) = require_test_model() else {
            return;
        };
        let text = "The quick brown fox";
        let embedding1 = embed_text(text, model).expect("first embed");
        let embedding2 = embed_text(text, model).expect("second embed");
        assert_eq!(embedding1, embedding2, "embedding should be deterministic");
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[ignore = "requires MLX model files on disk (see module doc comment for the expected path)"]
    fn test_embed_similarity() {
        let Some(model) = require_test_model() else {
            return;
        };
        let same_text = "machine learning on Apple Silicon";
        let different_text = "a recipe for chocolate chip cookies";

        let emb_same1 = embed_text(same_text, model).expect("embed 1");
        let emb_same2 = embed_text(same_text, model).expect("embed 2");
        let emb_diff = embed_text(different_text, model).expect("embed 3");

        let sim_identical = cosine_similarity(&emb_same1, &emb_same2);
        let sim_different = cosine_similarity(&emb_same1, &emb_diff);
        assert!(
            sim_identical > sim_different,
            "identical text should be more similar (identical={sim_identical}, different={sim_different})"
        );
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[ignore = "requires MLX model files on disk (see module doc comment for the expected path)"]
    fn test_embed_empty_string() {
        let Some(model) = require_test_model() else {
            return;
        };
        match embed_text("", model) {
            Ok(embedding) => {
                assert_eq!(embedding.len(), 384, "embedding should be 384-dimensional");
                let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!(
                    norm.is_finite(),
                    "empty-input embedding must have a finite norm"
                );
            }
            Err(_) => {
                // Also acceptable: an explicit error for empty input.
            }
        }
    }

    /// Real-hardware, real-model semantic test: proves the padding-mask fix, not just
    /// that the model runs. Before the fix, `MlxClient::embed` mean-pooled all 128
    /// fixed-padding positions unmasked; measured on this exact model, that compressed
    /// every pair toward ~0.85-0.94 with only a ~0.07-0.09 paraphrase/unrelated gap (the
    /// reviewer's report: cat/kitten paraphrase 0.904 vs cat/"stock markets" unrelated
    /// 0.853 — barely separated, and stocks/shares paraphrase 0.844 actually LOST to that
    /// unrelated pair). After masking, the same two pairs separate by ~0.59-0.63 (0.68ish
    /// vs 0.06-0.08). MARGIN sits between those two regimes, so this test is red on the
    /// old unmasked code and green on the fix — not just "greater than", which the old
    /// code's ~0.07 gap would already have satisfied.
    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[ignore = "requires MLX model files on disk (see module doc comment for the expected path)"]
    fn test_embed_semantic_similarity_survives_padding_mask() {
        let Some(model) = require_test_model() else {
            return;
        };
        const MARGIN: f32 = 0.3;

        let check_pair = |a: &str, b: &str, unrelated: &str| {
            let emb_a = embed_text(a, model).expect("embed a");
            let emb_b = embed_text(b, model).expect("embed b");
            let emb_unrelated = embed_text(unrelated, model).expect("embed unrelated");

            let sim_paraphrase = cosine_similarity(&emb_a, &emb_b);
            let sim_unrelated = cosine_similarity(&emb_a, &emb_unrelated);
            eprintln!(
                "paraphrase(\"{a}\", \"{b}\") = {sim_paraphrase}, unrelated(\"{a}\", \"{unrelated}\") = {sim_unrelated}"
            );
            assert!(
                sim_paraphrase > sim_unrelated + MARGIN,
                "paraphrase similarity ({sim_paraphrase}) should beat unrelated similarity \
                 ({sim_unrelated}) by at least {MARGIN} — a fixed-128-token padding mean-pool \
                 bug would let [PAD] tokens dominate and erase this margin"
            );
        };

        check_pair(
            "A cat sits on the mat",
            "A kitten is sitting on the rug",
            "Stock markets fell sharply today",
        );
        check_pair(
            "Shares dropped steeply this morning",
            "Stock markets fell sharply today",
            "A cat sits on the mat",
        );
    }

    #[test]
    fn stub_embed_returns_clear_error_off_apple_silicon() {
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            let err = embed_text("test", "test").unwrap_err();
            assert!(matches!(err, AiError::Backend(_)));
        }
    }

    /// Compute cosine similarity between two embedding vectors.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot_product / (norm_a * norm_b)
        }
    }
}
