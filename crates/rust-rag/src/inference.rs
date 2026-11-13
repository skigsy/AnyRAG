use anyhow::{Context, Result, ensure};
use ort::{session::Session, value::Tensor};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Mutex};
use tokenizers::{Tokenizer, TruncationParams};

mod cache;
mod rerank;
mod runtime;

pub use cache::{install, manifest};
pub(crate) use rerank::{DEPTH as RERANK_DEPTH, LocalReranker};
#[cfg(feature = "shared")]
pub(crate) use rerank::{Reranker, RerankerConfig};

const DIMENSIONS: usize = 384;
const MAX_TOKENS: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;

pub struct NativeEmbedder {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
    output_name: String,
    fingerprint: String,
}

impl NativeEmbedder {
    pub fn open(cache_dir: &Path, runtime_library: &Path, runtime_sha256: &str) -> Result<Self> {
        let runtime = runtime::VerifiedRuntime::open(runtime_library, runtime_sha256)?;
        let model = cache::VerifiedModel::load(cache_dir)?;
        let mut tokenizer = Tokenizer::from_bytes(model.tokenizer)
            .map_err(|_| anyhow::anyhow!("The verified BGE tokenizer could not load."))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|_| anyhow::anyhow!("The BGE tokenizer cannot apply its token limit."))?;
        tokenizer.with_padding(None);

        let environment = runtime.environment()?;
        let session = Session::builder(&environment)?
            .with_intra_threads(2)
            .map_err(|_| anyhow::anyhow!("Could not configure the BGE CPU session."))?
            .commit_from_memory(&model.onnx)
            .context("The verified BGE ONNX model could not load.")?;
        let output_name = session
            .outputs()
            .first()
            .context("BGE output is missing.")?
            .name()
            .to_owned();
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&fingerprint_inputs(
                &manifest(),
                runtime.sha256()
            ))?)
        );
        Ok(Self {
            tokenizer,
            session: Mutex::new(session),
            output_name,
            fingerprint,
        })
    }

    pub fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn embed(&self, text: &str, _query: bool) -> Result<Vec<f32>> {
        ensure!(
            text.len() <= MAX_TEXT_BYTES,
            "The text exceeds the native embedding input limit."
        );
        // The pinned BGE recipe encodes queries and passages with the same input format.
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|_| anyhow::anyhow!("BGE tokenization failed."))?;
        let tokens = encoding.len();
        ensure!(
            (1..=MAX_TOKENS).contains(&tokens),
            "Unexpected BGE input shape."
        );
        let tensor = |values: &[u32]| {
            Tensor::from_array((
                [1, tokens],
                values.iter().copied().map(i64::from).collect::<Vec<_>>(),
            ))
        };
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("The BGE session is unavailable."))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => tensor(encoding.get_ids())?,
            "attention_mask" => tensor(encoding.get_attention_mask())?,
            "token_type_ids" => tensor(encoding.get_type_ids())?,
        ])?;
        let (shape, values) = outputs
            .get(&self.output_name)
            .context("BGE output is missing.")?
            .try_extract_tensor::<f32>()?;
        pool_and_normalize(shape, values, tokens)
    }
}

fn fingerprint_inputs(manifest: &Value, runtime_sha256: &str) -> Value {
    serde_json::json!({
        "algorithm": "native-bge-wordpiece-cls-l2-512-v1",
        "tokenizer": "tokenizers-0.22.2",
        "runtime_sha256": runtime_sha256,
        "manifest": manifest,
    })
}

fn pool_and_normalize(shape: &[i64], values: &[f32], tokens: usize) -> Result<Vec<f32>> {
    ensure!(
        shape == [1, tokens as i64, DIMENSIONS as i64] || shape == [1, DIMENSIONS as i64],
        "Unexpected BGE output shape."
    );
    // CLS pooling selects the first token when the model returns per-token vectors.
    let row = values
        .get(..DIMENSIONS)
        .context("BGE output is incomplete.")?;
    let norm = row
        .iter()
        .map(|&component| f64::from(component).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(
        norm.is_finite() && norm > 0.0,
        "BGE returned an invalid vector."
    );
    Ok(row
        .iter()
        .map(|&component| (f64::from(component) / norm) as f32)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cls_pooling_uses_the_first_token_and_normalizes() {
        let mut values = vec![100.0; 2 * DIMENSIONS];
        values[..DIMENSIONS].fill(0.0);
        values[0] = 3.0;
        values[1] = 4.0;
        let output = pool_and_normalize(&[1, 2, DIMENSIONS as i64], &values, 2).unwrap();
        assert_eq!(&output[..2], &[0.6, 0.8]);
        assert_eq!(output.len(), DIMENSIONS);
        assert!(pool_and_normalize(&[2, 1, DIMENSIONS as i64], &values, 2).is_err());
        values[0] = f32::NAN;
        assert!(pool_and_normalize(&[1, 2, DIMENSIONS as i64], &values, 2).is_err());
    }

    #[test]
    #[ignore = "requires the explicitly installed pinned BGE model and ONNX Runtime library"]
    fn pinned_model_places_paraphrases_closer_than_unrelated_text() {
        let cache = std::env::var_os("RUST_RAG_TEST_MODEL_CACHE").expect("test model cache");
        let library = std::env::var_os("RUST_RAG_TEST_ONNX_RUNTIME").expect("test runtime library");
        let sha256 = std::env::var("RUST_RAG_TEST_ONNX_SHA256").expect("test runtime SHA-256");
        let model = NativeEmbedder::open(Path::new(&cache), Path::new(&library), &sha256).unwrap();
        let query = model.embed("How do I reset my password?", true).unwrap();
        let relevant = model
            .embed("Recover your account by choosing a new password.", false)
            .unwrap();
        let unrelated = model
            .embed("The blue whale is the largest animal in the ocean.", false)
            .unwrap();
        let cosine = |other: &[f32]| {
            query
                .iter()
                .zip(other)
                .map(|(left, right)| left * right)
                .sum::<f32>()
        };
        assert!((cosine(&query) - 1.0).abs() < 1e-5);
        assert!(cosine(&relevant) > cosine(&unrelated) + 0.1);
    }
}
