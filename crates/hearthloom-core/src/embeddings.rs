//! Local ONNX embeddings (all-MiniLM-L6-v2 via tract) — the HearthFS
//! pattern, shared discipline, zero HearthFS dependency. Fully local:
//! no provider, no network, ever.
//!
//! 384-dim, 128-token window, mean-pooled and L2-normalized. `embed` is
//! `&self` and safe to call concurrently (tract shares weights and builds
//! per-call state; the tokenizer is also `&self`).

use std::path::Path;

use anyhow::{Context, Result};
use tract_onnx::prelude::*;

type Model = Arc<TypedRunnableModel>;

const MAX_TOKENS: usize = 128;

/// The embedding model's name, as the usage ledger records its runs.
pub const MODEL_NAME: &str = "all-MiniLM-L6-v2";

pub struct Embedder {
    model: Model,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
}

impl Embedder {
    /// Load `all-MiniLM-L6-v2.onnx` + `tokenizer.json` from a directory
    /// (default `~/.hearthloom/models`).
    pub fn load(model_dir: &Path) -> Result<Self> {
        let model_path = model_dir.join("all-MiniLM-L6-v2.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");
        anyhow::ensure!(
            model_path.exists() && tokenizer_path.exists(),
            "embedding model not found in {} (expected all-MiniLM-L6-v2.onnx + tokenizer.json)",
            model_dir.display()
        );

        let model = tract_onnx::onnx()
            .model_for_path(&model_path)
            .context("loading ONNX model")?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(i64::datum_type(), [1, MAX_TOKENS as i64]),
            )?
            .with_input_fact(
                1,
                InferenceFact::dt_shape(i64::datum_type(), [1, MAX_TOKENS as i64]),
            )?
            .with_input_fact(
                2,
                InferenceFact::dt_shape(i64::datum_type(), [1, MAX_TOKENS as i64]),
            )?
            .into_optimized()
            .context("optimizing model")?
            .into_runnable()
            .context("making model runnable")?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer: {e}"))?;

        Ok(Embedder {
            model,
            tokenizer,
            dim: 384,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed one text (truncated to the 128-token window).
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let mut input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mut attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();
        let mut token_type_ids: Vec<i64> =
            encoding.get_type_ids().iter().map(|&t| t as i64).collect();
        input_ids.truncate(MAX_TOKENS);
        attention_mask.truncate(MAX_TOKENS);
        token_type_ids.truncate(MAX_TOKENS);
        while input_ids.len() < MAX_TOKENS {
            input_ids.push(0);
            attention_mask.push(0);
            token_type_ids.push(0);
        }

        let input_ids =
            tract_ndarray::Array2::from_shape_vec((1, MAX_TOKENS), input_ids)?.into_tensor();
        let attention_mask =
            tract_ndarray::Array2::from_shape_vec((1, MAX_TOKENS), attention_mask)?.into_tensor();
        let token_type_ids =
            tract_ndarray::Array2::from_shape_vec((1, MAX_TOKENS), token_type_ids)?.into_tensor();

        let outputs = self.model.run(tvec![
            input_ids.into(),
            attention_mask.into(),
            token_type_ids.into(),
        ])?;

        // Output is [1, seq, hidden]: mean-pool the sequence, L2-normalize.
        let output = outputs[0]
            .to_plain_array_view::<f32>()
            .context("extracting output tensor")?;
        let (seq_len, hidden) = (output.shape()[1], output.shape()[2]);
        let mut pooled = vec![0.0f32; hidden];
        for i in 0..seq_len {
            for j in 0..hidden {
                pooled[j] += output[[0, i, j]];
            }
        }
        for v in &mut pooled {
            *v /= seq_len as f32;
        }
        let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut pooled {
                *v /= norm;
            }
        }
        Ok(pooled)
    }
}

/// Cosine similarity of two L2-normalized vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn bytes_to_floats(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_and_roundtrip() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
        assert_eq!(bytes_to_floats(&floats_to_bytes(&a)), a);
    }

    #[test]
    fn test_embedder_if_model_present() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let dir = Path::new(&home).join(".hearthloom/models");
        if !dir.join("all-MiniLM-L6-v2.onnx").exists() {
            eprintln!("skipping: no model at {}", dir.display());
            return;
        }
        let embedder = Embedder::load(&dir).unwrap();
        let auth = embedder.embed("authentication module").unwrap();
        let login = embedder.embed("user login system").unwrap();
        let soup = embedder.embed("tomato soup recipe").unwrap();
        assert_eq!(auth.len(), 384);
        assert!(cosine_similarity(&auth, &login) > cosine_similarity(&auth, &soup));
    }
}
