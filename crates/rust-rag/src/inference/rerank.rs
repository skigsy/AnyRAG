use super::{cache, runtime};
use anyhow::{Context, Result, ensure};
use ort::{
    session::{RunOptions, Session},
    value::Tensor,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams, utils::padding::pad_encodings};

pub(crate) const DEPTH: usize = 20;
const MAX_PROFILE_BYTES: usize = 8 * 1024;
const MAX_TOKENS: usize = 256;
const MAX_PASSAGE_BYTES: usize = 16 * 1024;
const BUDGET: Duration = Duration::from_millis(120);
const MODEL_SHA256: &str = "2c7f4c682cf9320acb3748c619b2e21d24ef460e2f96b98b0ab6b190c5eb2ca9";
const TOKENIZER_SHA256: &str = "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66";
const ETTIN_MODEL_SHA256: &str = "b29f789c1c22abf20c0a47892762dbbfaceda72605b3089eb2685ddfbb213bfa";
const ETTIN_TOKENIZER_SHA256: &str =
    "28c5e078e4c52aa37cf0e6de1a212878f3dbd58dd1c70466298efe0b6b86db35";
const ETTIN_SCORE_DEPTH: usize = 15;
const ETTIN_MAX_TOKENS: usize = 68;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
enum RerankerModel {
    #[default]
    #[serde(rename = "minilm-l2-v2")]
    MiniLm,
    #[serde(rename = "ettin-150m")]
    Ettin150M,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RerankerConfig {
    #[serde(default)]
    model: RerankerModel,
    pub(crate) model_dir: PathBuf,
    pub(crate) runtime_library: PathBuf,
    pub(crate) runtime_sha256: String,
}

pub(crate) struct LocalReranker(Result<Reranker, &'static str>);

impl LocalReranker {
    pub(crate) fn open(profile: &Path) -> Self {
        let mut remaining = MAX_PROFILE_BYTES;
        let config = crate::files::read(profile, &mut remaining)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RerankerConfig>(&bytes).ok())
            .ok_or("invalid_profile");
        Self(config.and_then(|config| Reranker::open(&config).map_err(|_| "unavailable")))
    }

    pub(crate) fn apply(&self, question: &str, result: &mut Value) {
        let status = match (&self.0, result["evidence"].as_array_mut()) {
            (Ok(reranker), Some(evidence)) => reranker.reorder_inner(question, evidence, true),
            (Err(reason), _) => fallback(reason),
            (Ok(_), None) => fallback("unavailable"),
        };
        result["reranking"] = status;
    }
}

pub(crate) struct Reranker {
    sender: SyncSender<Job>,
    model: RerankerModel,
}

struct Job {
    question: String,
    passages: Vec<String>,
    deadline: Instant,
    options: Arc<RunOptions>,
    result: mpsc::Sender<Result<Vec<f32>>>,
}

impl Reranker {
    pub(crate) fn open(config: &RerankerConfig) -> Result<Self> {
        #[cfg(target_arch = "x86_64")]
        let supported = std::is_x86_feature_detected!("avx2");
        #[cfg(not(target_arch = "x86_64"))]
        let supported = false;
        ensure!(
            supported,
            "The pinned reranker requires an x86-64 build with AVX2."
        );
        let mut model = CrossEncoder::open(config)?;
        // A busy worker must not retain a second request's passage text.
        let (sender, receiver) = mpsc::sync_channel::<Job>(0);
        thread::Builder::new()
            .name("rust-rag-reranker".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if Instant::now() < job.deadline {
                        let scores = model.score(&job.question, &job.passages, &job.options);
                        let _ = job.result.send(scores);
                    }
                }
            })?;
        Ok(Self {
            sender,
            model: config.model,
        })
    }

    #[cfg(feature = "shared")]
    pub(crate) fn reorder(&self, question: &str, evidence: &mut [Value]) -> Value {
        self.reorder_inner(question, evidence, false)
    }

    fn reorder_inner(&self, question: &str, evidence: &mut [Value], local: bool) -> Value {
        if evidence.len() < 2 {
            return json!({"status":"skipped","reason":"insufficient_candidates"});
        }
        if evidence.len() > DEPTH {
            return fallback("candidate_limit");
        }
        let scored_count = if self.model == RerankerModel::Ettin150M {
            if contains_exact_identifier(question, evidence) {
                return json!({"status":"applied","reason":"preserved_identifier_order"});
            }
            evidence.len().min(ETTIN_SCORE_DEPTH)
        } else {
            evidence.len()
        };
        let Some(passages) = evidence[..scored_count]
            .iter()
            .map(|hit| {
                let text = hit["text"].as_str()?;
                (text.len() <= MAX_PASSAGE_BYTES).then(|| text.to_owned())
            })
            .collect::<Option<Vec<_>>>()
        else {
            return fallback("input_limit");
        };
        let Ok(options) = RunOptions::new().map(Arc::new) else {
            return fallback("unavailable");
        };
        let (result, reply) = mpsc::channel();
        let deadline = Instant::now() + BUDGET;
        let mut job = Job {
            question: question.to_owned(),
            passages,
            deadline,
            options: options.clone(),
            result,
        };
        loop {
            match self.sender.try_send(job) {
                Ok(()) => break,
                Err(TrySendError::Full(pending)) if local && Instant::now() < deadline => {
                    // Local callers are serial. Allow a newly spawned receiver to
                    // reach its first receive without buffering passage text.
                    job = pending;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Full(_)) => return fallback("busy"),
                Err(TrySendError::Disconnected(_)) => return fallback("unavailable"),
            }
        }
        match reply.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(scores)) if apply_scores(&mut evidence[..scored_count], &scores) => {
                json!({"status":"applied"})
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = options.terminate();
                fallback("timeout")
            }
            _ => fallback("invalid_scores"),
        }
    }
}

fn contains_exact_identifier(question: &str, evidence: &[Value]) -> bool {
    fn split(text: &str) -> impl Iterator<Item = &str> {
        text.split(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_' | '.' | '/'))
            .map(|word| word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric()))
            .filter(|word| !word.is_empty())
    }

    question.split_ascii_whitespace().count() > 0
        && split(question).any(|term| {
            term.bytes().filter(u8::is_ascii_digit).count() >= 2
                && term.bytes().any(|byte| byte.is_ascii_alphabetic())
                && evidence.iter().any(|hit| {
                    hit["text"]
                        .as_str()
                        .is_some_and(|text| split(text).any(|word| word.eq_ignore_ascii_case(term)))
                })
        })
}

fn fallback(reason: &'static str) -> Value {
    json!({"status":"fallback","reason":reason})
}

fn apply_scores(evidence: &mut [Value], scores: &[f32]) -> bool {
    if scores.len() != evidence.len() || scores.iter().any(|score| !score.is_finite()) {
        return false;
    }
    let mut order: Vec<_> = (0..scores.len()).collect();
    order.sort_by(|&left, &right| scores[right].total_cmp(&scores[left]));
    let sorted: Vec<_> = order
        .into_iter()
        .map(|index| evidence[index].take())
        .collect();
    for (item, sorted) in evidence.iter_mut().zip(sorted) {
        *item = sorted;
    }
    true
}

struct CrossEncoder {
    tokenizer: Tokenizer,
    session: Session,
}

impl CrossEncoder {
    fn open(config: &RerankerConfig) -> Result<Self> {
        ensure!(
            config.model_dir.is_absolute(),
            "Use an absolute reranker model directory."
        );
        let model_dir = crate::files::state_directory(&config.model_dir, false)?;
        let (model_size, model_hash, tokenizer_size, tokenizer_hash, max_tokens) =
            match config.model {
                RerankerModel::MiniLm => (
                    15_794_398,
                    MODEL_SHA256,
                    711_396,
                    TOKENIZER_SHA256,
                    MAX_TOKENS,
                ),
                RerankerModel::Ettin150M => (
                    152_997_689,
                    ETTIN_MODEL_SHA256,
                    3_583_327,
                    ETTIN_TOKENIZER_SHA256,
                    ETTIN_MAX_TOKENS,
                ),
            };
        let tokenizer = cache::read_local_file(
            &model_dir.join("tokenizer.json"),
            tokenizer_size,
            tokenizer_hash,
        )
        .context("The pinned reranker tokenizer is missing or changed.")?;
        let onnx = cache::read_local_file(&model_dir.join("model.onnx"), model_size, model_hash)
            .context("The pinned reranker model is missing or changed.")?;
        let mut tokenizer = Tokenizer::from_bytes(tokenizer)
            .map_err(|_| anyhow::anyhow!("The pinned reranker tokenizer could not load."))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_tokens,
                ..Default::default()
            }))
            .map_err(|_| anyhow::anyhow!("Could not set the reranker token limit."))?;
        tokenizer.with_padding(None);
        let runtime =
            runtime::VerifiedRuntime::open(&config.runtime_library, &config.runtime_sha256)?;
        let session = Session::builder(&runtime.environment()?)?
            .with_intra_threads(4)
            .map_err(|_| anyhow::anyhow!("Could not configure the reranker CPU session."))?
            .commit_from_memory(&onnx)
            .context("The verified reranker ONNX model could not load.")?;
        Ok(Self { tokenizer, session })
    }

    fn score(
        &mut self,
        question: &str,
        passages: &[String],
        options: &RunOptions,
    ) -> Result<Vec<f32>> {
        let mut encodings = passages
            .iter()
            .map(|text| {
                self.tokenizer
                    .encode((question, text.as_str()), true)
                    .map_err(|_| anyhow::anyhow!("Reranker tokenization failed."))
            })
            .collect::<Result<Vec<_>>>()?;
        pad_encodings(&mut encodings, &PaddingParams::default())
            .map_err(|_| anyhow::anyhow!("Reranker padding failed."))?;
        let tokens = encodings.first().context("Reranker input is empty.")?.len();
        let tensor = |field: fn(&tokenizers::Encoding) -> &[u32]| {
            Tensor::from_array((
                [encodings.len(), tokens],
                encodings
                    .iter()
                    .flat_map(field)
                    .copied()
                    .map(i64::from)
                    .collect::<Vec<_>>(),
            ))
        };
        let mut inputs = ort::inputs![
            "input_ids" => tensor(tokenizers::Encoding::get_ids)?,
            "attention_mask" => tensor(tokenizers::Encoding::get_attention_mask)?,
        ];
        if self
            .session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids")
        {
            inputs.push((
                "token_type_ids".into(),
                tensor(tokenizers::Encoding::get_type_ids)?.into(),
            ));
        }
        let outputs = self.session.run_with_options(inputs, options)?;
        let (shape, scores) = outputs
            .get("logits")
            .context("Reranker logits are missing.")?
            .try_extract_tensor::<f32>()?;
        ensure!(
            **shape == [passages.len() as i64, 1],
            "Unexpected reranker output shape."
        );
        Ok(scores.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_selection_preserves_legacy_profiles_and_rejects_unknown_models() -> Result<()> {
        let mut config = json!({
            "model_dir":"models","runtime_library":"runtime","runtime_sha256":"hash"
        });
        let parse = |value: &Value| serde_json::from_value::<RerankerConfig>(value.clone());
        assert!(matches!(parse(&config)?.model, RerankerModel::MiniLm));
        config["model"] = json!("ettin-150m");
        assert!(matches!(parse(&config)?.model, RerankerModel::Ettin150M));
        config["model"] = json!("unknown");
        assert!(parse(&config).is_err());
        Ok(())
    }

    #[test]
    fn ettin_keeps_matching_identifiers_in_the_first_stage_order() {
        let (sender, _receiver) = mpsc::sync_channel(0);
        let reranker = Reranker {
            sender,
            model: RerankerModel::Ettin150M,
        };
        let mut evidence = [
            json!({"chunk_id":"a","text":"Valve AB-204 is available."}),
            json!({"chunk_id":"b","text":"Spare AB-2041 is retired."}),
        ];
        let before = evidence.clone();
        assert_eq!(
            reranker.reorder_inner("Show ab-204.", &mut evidence, false),
            json!({"status":"applied","reason":"preserved_identifier_order"})
        );
        assert_eq!(evidence, before);
        assert!(!contains_exact_identifier("Show AB-20.", &evidence));
    }

    #[test]
    fn oversized_local_profile_keeps_existing_evidence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let profile = directory.path().join("reranker.json");
        std::fs::write(&profile, vec![b' '; MAX_PROFILE_BYTES + 1])?;
        let evidence = json!([
            {"chunk_id":"b","page":2,"start":4,"end":9,"text":"sprig"},
            {"chunk_id":"a","page":1,"start":8,"end":13,"text":"cedar"}
        ]);
        let mut result = json!({"status":"evidence","evidence":evidence});
        LocalReranker::open(&profile).apply("cedar", &mut result);
        assert_eq!(result["evidence"], evidence);
        assert_eq!(
            result["reranking"],
            json!({"status":"fallback","reason":"invalid_profile"})
        );
        Ok(())
    }

    #[test]
    fn reorder_preserves_citations_and_retains_the_first_stage_on_bad_scores() {
        let a = json!({"chunk_id":"a","start":3,"end":6,"text":"one"});
        let b = json!({"chunk_id":"b","start":9,"end":12,"text":"two"});
        let c = json!({"chunk_id":"c","start":16,"end":21,"text":"three"});
        let mut evidence = [a.clone(), b.clone(), c.clone()];
        assert!(!apply_scores(&mut evidence, &[0.0, 1.0]));
        assert!(!apply_scores(&mut evidence, &[0.0, f32::NAN, 1.0]));
        assert_eq!(evidence, [a.clone(), b.clone(), c.clone()]);
        assert!(apply_scores(&mut evidence, &[0.0, 1.0, 1.0]));
        assert_eq!(evidence, [b, c, a]);
    }
}
