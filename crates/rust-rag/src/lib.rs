#![deny(unsafe_code)]

#[cfg(feature = "aws")]
mod cloud;
#[cfg(feature = "dashboard")]
mod dashboard;
#[cfg(feature = "model")]
mod demo;
#[cfg(any(feature = "pdf", feature = "docx", feature = "xlsx"))]
mod document_worker;
#[cfg(feature = "docx")]
mod docx;
mod enrichment;
mod files;
mod graph;
mod guide;
#[cfg(feature = "inference")]
mod inference;
#[cfg(feature = "mcp")]
mod mcp;
#[cfg(feature = "model")]
mod model;
mod ocr;
#[cfg(feature = "pdf")]
mod pdf;
mod sdk;
#[cfg(feature = "shared")]
mod shared;
mod store;
mod vector;
#[cfg(feature = "xlsx")]
mod xlsx;

pub use sdk::Index;
pub use store::Mode as RetrievalMode;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Rust RAG. Search your sources, follow relationships, and get cited evidence."
)]
struct Cli {
    /// Private, unsynced index folder
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Return machine-readable JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct EmbeddingOptions {
    /// Explicit OpenAI-compatible embedding endpoint configuration
    #[arg(long)]
    embedding_endpoint: Option<PathBuf>,
    /// Private directory containing the verified local embedding model
    #[cfg(feature = "inference")]
    #[arg(long, requires_all = ["ort_library", "ort_sha256"], conflicts_with = "embedding_endpoint")]
    model_cache: Option<PathBuf>,
    /// Absolute path to the ONNX Runtime shared library supplied by the operator
    #[cfg(feature = "inference")]
    #[arg(long, requires = "model_cache")]
    ort_library: Option<PathBuf>,
    /// SHA-256 of the operator-verified ONNX Runtime library
    #[cfg(feature = "inference")]
    #[arg(long, requires = "model_cache")]
    ort_sha256: Option<String>,
}

impl EmbeddingOptions {
    fn requested(&self) -> bool {
        if self.embedding_endpoint.is_some() {
            return true;
        }
        #[cfg(feature = "inference")]
        return self.model_cache.is_some();
        #[cfg(not(feature = "inference"))]
        false
    }

    #[cfg(feature = "model")]
    fn open(&self) -> Result<Box<dyn Embedder>> {
        #[cfg(feature = "inference")]
        if let (Some(cache), Some(runtime), Some(digest)) =
            (&self.model_cache, &self.ort_library, &self.ort_sha256)
        {
            return Ok(Box::new(inference::NativeEmbedder::open(
                cache, runtime, digest,
            )?));
        }
        let path = self.embedding_endpoint.as_ref().context(
            "Select --embedding-endpoint or a local model (with the inference feature).",
        )?;
        Ok(Box::new(model::EmbeddingEndpoint::open(path)?))
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create an empty personal index
    Init {
        /// Enable the advanced index format and save a rollback copy of existing state
        #[arg(long)]
        upgrade: bool,
        /// Apply a trusted versioned local index profile
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Create a fresh OCR index bound to this trusted Linux worker profile
        #[arg(long, conflicts_with_all = ["upgrade", "profile"])]
        ocr_profile: Option<PathBuf>,
    },
    /// Check bundled SQLite and the current index
    Doctor,
    /// List the capabilities compiled into this binary
    Capabilities,
    /// Report current personal index coverage without changing the index
    #[cfg(feature = "dashboard")]
    Health {
        /// Write a self-contained report to a new HTML file
        #[arg(long)]
        html: Option<PathBuf>,
    },
    /// Open a live browser view of the local personal index
    #[cfg(feature = "dashboard")]
    Dashboard {
        /// Print the address and access code without opening the default browser
        #[arg(long)]
        no_open: bool,
        /// Show pending graph proposals and their source quotes in this browser session
        #[arg(long)]
        review_queue: bool,
    },
    /// Manage opt-in activity history for this personal index
    #[cfg(feature = "dashboard")]
    History {
        #[arg(value_enum)]
        action: dashboard::history::Action,
    },
    /// Walk through a synthetic Graph RAG example without any model or API key
    #[cfg(feature = "model")]
    Demo,
    /// Recommend a starting profile for a workload
    Plan {
        #[arg(value_enum)]
        profile: Option<guide::Profile>,
    },
    /// Serve read-only search for one existing personal index over stdio
    #[cfg(feature = "mcp")]
    Mcp {
        /// Trusted local reranker profile for opt-in search requests
        #[cfg(feature = "inference")]
        #[arg(long)]
        reranker_profile: Option<PathBuf>,
    },
    /// Run the authenticated HTTP service
    #[cfg(feature = "shared")]
    Serve {
        #[arg(long)]
        profile: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: std::net::SocketAddr,
    },
    /// Validate an authenticated service profile
    #[cfg(feature = "shared")]
    ServiceDoctor {
        #[arg(long)]
        profile: PathBuf,
    },
    /// Show the model's pinned source, files, sizes and hashes
    #[cfg(feature = "inference")]
    ModelManifest,
    /// Explicitly download or import the pinned embedding model
    #[cfg(feature = "inference")]
    ModelInstall {
        #[arg(long)]
        cache: PathBuf,
        #[arg(
            long,
            conflicts_with = "download",
            required_unless_present = "download"
        )]
        from: Option<PathBuf>,
        #[arg(long, required_unless_present = "from")]
        download: bool,
    },
    /// Index a folder, skipping Obsidian metadata and trash
    Ingest {
        folder: PathBuf,
        /// Remove missing sources after a complete scan
        #[arg(long)]
        sync: bool,
        /// Exit unsuccessfully when a sync reports deferred work or failures
        #[arg(long, requires = "sync")]
        require_complete: bool,
        /// Include each inspected file's outcome and any skip or failure reason
        #[arg(long)]
        review: bool,
        /// Read text-layer PDFs with the optional native PDF reader
        #[arg(long)]
        pdf: bool,
        /// Read PDFs with the Linux worker bound at initialization
        #[arg(long, conflicts_with = "pdf")]
        ocr_profile: Option<PathBuf>,
        /// Read Word main-body text with the optional native DOCX reader
        #[arg(long)]
        docx: bool,
        /// Read literal worksheet values with the optional Excel reader
        #[arg(long)]
        xlsx: bool,
    },
    /// Find source passages for a question
    Query {
        question: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Expand strong matches through the indexed typed graph
        #[arg(long)]
        graph: bool,
        /// Select how passages are ranked
        #[arg(long, value_enum, default_value = "lexical")]
        mode: store::Mode,
        /// Explain which stages ran and why each passage was returned
        #[arg(long)]
        explain: bool,
        #[command(flatten)]
        embedding: EmbeddingOptions,
        /// Rerank this query in process with a trusted local profile
        #[cfg(feature = "inference")]
        #[arg(long)]
        reranker_profile: Option<PathBuf>,
    },
    /// Reopen a saved passage in the current personal index
    Citation {
        chunk_id: String,
        /// Revision from the saved query evidence
        #[arg(long)]
        revision: String,
    },
    /// Index embeddings for active passages using an explicit endpoint
    #[cfg(feature = "model")]
    Embed {
        #[command(flatten)]
        embedding: EmbeddingOptions,
        #[arg(long, default_value_t = 32)]
        limit: usize,
        /// Reset deferred and quarantined work for this model
        #[arg(long)]
        retry: bool,
    },
    /// Add source-anchored typed relationships using an explicit model endpoint
    #[cfg(feature = "model")]
    ExtractGraph {
        #[arg(long)]
        model_endpoint: PathBuf,
        /// Stage source-anchored relationships from local Markdown and text for confirmation
        #[arg(long)]
        source_clauses: bool,
        #[arg(long, default_value_t = 32)]
        limit: usize,
        /// Reset deferred and quarantined work for this model
        #[arg(long)]
        retry: bool,
    },
    /// List or decide pending local graph relationships
    #[cfg(feature = "model")]
    GraphReview {
        #[command(subcommand)]
        action: GraphReviewAction,
    },
    /// Ask an explicitly configured model to answer with citations
    #[cfg(feature = "model")]
    Answer {
        question: String,
        #[arg(long)]
        model_endpoint: PathBuf,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        graph: bool,
        #[arg(long, value_enum, default_value = "lexical")]
        mode: store::Mode,
        #[command(flatten)]
        embedding: EmbeddingOptions,
    },
    /// Show the folder and index counts
    Status,
    /// Export all active indexed passages to a new private JSONL file
    Export {
        #[arg(long)]
        output: PathBuf,
    },
    /// Remove evidence from the index and preserve the original files
    Remove {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        source_id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Permanently discard local pre-upgrade rollback files
    DiscardRollback {
        /// Acknowledge that these pre-upgrade copies cannot be recovered by Rust RAG
        #[arg(long)]
        confirm: bool,
    },
    #[cfg(feature = "pdf")]
    #[command(name = "__pdf-worker", hide = true)]
    PdfWorker,
    #[cfg(feature = "xlsx")]
    #[command(name = "__xlsx-worker", hide = true)]
    XlsxWorker,
    #[cfg(feature = "docx")]
    #[command(name = "__docx-worker", hide = true)]
    DocxWorker,
}

#[cfg(feature = "model")]
#[derive(Subcommand)]
enum GraphReviewAction {
    /// Show pending relationships with their exact source citations
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Publish one pending relationship after checking its source revision
    Approve { proposal_id: String },
    /// Remove one pending relationship from the queue
    Dismiss { proposal_id: String },
}

/// A rejected answer keeps its error message and exit status, and carries the
/// text-free delivery manifest so `--json` output still reports what was sent.
#[cfg(feature = "model")]
#[derive(Debug)]
struct AnswerFailure {
    message: String,
    delivery: Value,
}

#[cfg(feature = "model")]
impl std::fmt::Display for AnswerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(feature = "model")]
impl std::error::Error for AnswerFailure {}

fn run(cli: Cli) -> Result<Value> {
    match &cli.command {
        Command::Capabilities => return Ok(guide::capabilities()),
        #[cfg(feature = "model")]
        Command::Demo => return demo::run(),
        Command::Plan { profile } => return Ok(guide::plan(*profile)),
        #[cfg(feature = "shared")]
        Command::Serve { profile, bind } => {
            shared::serve(profile, *bind)?;
            return Ok(json!({"status": "stopped"}));
        }
        #[cfg(feature = "shared")]
        Command::ServiceDoctor { profile } => return shared::doctor(profile),
        #[cfg(feature = "inference")]
        Command::ModelManifest => return Ok(inference::manifest()),
        #[cfg(feature = "inference")]
        Command::ModelInstall { cache, from, .. } => {
            return inference::install(cache, from.as_deref());
        }
        _ => {}
    }
    let init = matches!(cli.command, Command::Init { .. });
    let upgrade = match &cli.command {
        Command::Init {
            upgrade, profile, ..
        } => {
            *upgrade
                | profile
                    .as_deref()
                    .map(guide::advanced_profile)
                    .transpose()?
                    .unwrap_or(false)
        }
        _ => false,
    };
    let worker = match &cli.command {
        Command::Init { ocr_profile, .. } | Command::Ingest { ocr_profile, .. } => {
            ocr_profile.as_deref().map(ocr::Worker::load).transpose()?
        }
        _ => None,
    };
    let requested = match cli.state_dir {
        Some(path) => path,
        None => files::default_state()?,
    };
    #[cfg(feature = "dashboard")]
    match &cli.command {
        Command::Health { html } => return dashboard::health(&requested, html.as_deref()),
        Command::Dashboard {
            no_open,
            review_queue,
        } => {
            dashboard::serve(&requested, !*no_open, *review_queue)?;
            return Ok(json!({"status": "stopped"}));
        }
        Command::History { action } => return dashboard::history_control(&requested, *action),
        _ => {}
    }
    let state = files::state_directory(&requested, init)?;
    let mut db = store::open_with_ocr(&state, init, worker.as_ref().map(ocr::Worker::fingerprint))?;
    match cli.command {
        Command::Init { .. } | Command::Status | Command::Doctor => {
            let backup = if upgrade {
                store::upgrade(&mut db, &state)?
            } else {
                None
            };
            let mut result = store::status(&db)?;
            result["state_dir"] = json!(files::display_path(&state)?);
            result["retained_rollback_copies"] = json!(files::rollback_copy_count(&state).ok());
            if let Some(backup) = backup {
                result["upgrade_backup"] = json!(files::display_path(&backup)?);
            }
            if matches!(cli.command, Command::Doctor) {
                // Opening the store also checks the persisted FTS secure-delete setting.
                let fts5: bool = db.query_row(
                    "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                    [],
                    |row| row.get(0),
                )?;
                ensure!(fts5, "The SQLite build lacks FTS5.");
                result["sqlite"] = json!({
                    "version": rusqlite::version(), "fts5": fts5, "secure_delete": true,
                    "fts_secure_delete": true, "journal_mode": "delete"
                });
                result["status"] = json!("ready");
            }
            Ok(result)
        }
        Command::Ingest {
            folder,
            sync,
            review,
            pdf,
            docx,
            xlsx,
            ..
        } => {
            #[cfg(feature = "dashboard")]
            let history = dashboard::history::IngestRecorder::start(&state, sync);
            let result = (|| {
                let root = files::directory(&folder)?;
                ensure!(
                    !state.starts_with(&root) && !root.starts_with(&state),
                    "Use separate folders for the sources and the index."
                );
                store::ingest_with_options(
                    &mut db,
                    &root,
                    store::LocalIngestOptions {
                        sync,
                        pdf,
                        docx,
                        xlsx,
                        review,
                    },
                    &mut store::OcrBatch::new(worker.as_ref()),
                )
            })();
            #[cfg(feature = "dashboard")]
            if let Some(history) = history {
                history.finish(&result);
            }
            result
        }
        Command::Query {
            question,
            limit,
            graph,
            mode,
            explain,
            embedding,
            #[cfg(feature = "inference")]
            reranker_profile,
        } => {
            #[cfg(feature = "dashboard")]
            let mut history = dashboard::history::QueryRecorder::start(&state, mode, graph);
            let result = (|| {
                #[cfg(feature = "inference")]
                ensure!(
                    reranker_profile.is_none() || limit <= inference::RERANK_DEPTH,
                    "Reranking supports up to {} results.",
                    inference::RERANK_DEPTH
                );
                #[cfg(feature = "dashboard")]
                let started = history.as_ref().map(|_| std::time::Instant::now());
                let retrieved = retrieve(&mut db, &question, limit, graph, mode, &embedding);
                #[cfg(feature = "dashboard")]
                if let (Some(history), Some(started)) = (&mut history, started) {
                    history.retrieval_finished(started);
                }
                let mut result = retrieved?;
                let first_stage = if explain {
                    result["evidence"]
                        .as_array()
                        .context("Missing query evidence.")?
                        .iter()
                        .map(|hit| hit["chunk_id"].clone())
                        .collect()
                } else {
                    Vec::new()
                };
                #[cfg(feature = "inference")]
                if let Some(profile) = reranker_profile {
                    #[cfg(feature = "dashboard")]
                    let started = history.as_ref().map(|_| std::time::Instant::now());
                    inference::LocalReranker::open(&profile).apply(&question, &mut result);
                    #[cfg(feature = "dashboard")]
                    if let (Some(history), Some(started)) = (&mut history, started) {
                        history.rerank_finished(started);
                    }
                }
                if explain {
                    result["explanation"] = explain_query(&result, &first_stage);
                }
                Ok(result)
            })();
            #[cfg(feature = "dashboard")]
            if let Some(history) = history {
                history.finish(&result);
            }
            result
        }
        Command::Citation { chunk_id, revision } => {
            store::open_citation(&mut db, &chunk_id, &revision)
        }
        #[cfg(feature = "model")]
        Command::Embed {
            embedding,
            limit,
            retry,
        } => {
            store::require_enrichment(&db)?;
            let embedder = embedding.open()?;
            index_vectors(&mut db, embedder.as_ref(), limit, retry)
        }
        #[cfg(feature = "model")]
        Command::ExtractGraph {
            model_endpoint,
            source_clauses,
            limit,
            retry,
        } => {
            store::require_enrichment(&db)?;
            let endpoint = model::ModelEndpoint::open(&model_endpoint)?;
            extract_graph(&mut db, &endpoint, limit, retry, source_clauses)
        }
        #[cfg(feature = "model")]
        Command::GraphReview { action } => match action {
            GraphReviewAction::List { limit } => graph::review_queue(&db, limit),
            GraphReviewAction::Approve { proposal_id } => {
                graph::decide_proposal(&mut db, &proposal_id, true)
            }
            GraphReviewAction::Dismiss { proposal_id } => {
                graph::decide_proposal(&mut db, &proposal_id, false)
            }
        },
        #[cfg(feature = "model")]
        Command::Answer {
            question,
            model_endpoint,
            limit,
            graph,
            mode,
            embedding,
        } => {
            let result = retrieve(&mut db, &question, limit, graph, mode, &embedding)?;
            let evidence = result["evidence"]
                .as_array()
                .context("Missing query evidence.")?;
            let answer = if evidence.is_empty() {
                json!({"status": "insufficient_evidence", "answer": null, "citations": [],
                    "cited_evidence": [], "delivery": {"considered": 0, "sent": [], "omitted": []}})
            } else {
                let attempt = model::ModelEndpoint::open(&model_endpoint)?
                    .answer_attempt(&question, evidence)?;
                let mut answer = attempt.result.map_err(|error| AnswerFailure {
                    message: error.to_string(),
                    delivery: attempt.delivery.clone(),
                })?;
                answer["delivery"] = attempt.delivery.clone();
                store::recheck_cited(&mut db, answer).map_err(|error| AnswerFailure {
                    message: error.to_string(),
                    delivery: attempt.delivery,
                })?
            };
            let mut output = json!({
                "schema_version": 1, "status": answer["status"], "answer": answer["answer"],
                "citations": answer["citations"], "cited_evidence": answer["cited_evidence"],
                "answer_support": "unverified",
                "answer_generated": answer["status"] == "answered",
                "model_input": "question and selected passages may go to the configured chat endpoint",
                "delivery": answer["delivery"],
                "embedding_input": result["embedding_input"],
                "evidence": evidence, "graph_coverage": result["graph_coverage"],
                "vector_coverage": result["vector_coverage"],
            });
            if let Some(reason) = answer.get("reason") {
                output["reason"] = reason.clone();
            }
            Ok(output)
        }
        Command::Remove { source_id, all: _ } => {
            let removed = store::remove(&mut db, source_id.as_deref())?;
            db.close()
                .map_err(|(_, error)| error)
                .context("Removal committed, but closing the index failed.")?;
            let rollback_copies = files::rollback_copy_count(&state).ok();
            Ok(json!({
                "schema_version": 1, "removed_sources": removed,
                "logical_removal": true, "source_files_changed": false,
                "purge": {"secure_delete": true, "fts_secure_delete": true,
                    "retained_rollback_copies": rollback_copies,
                    "rollback_may_retain_evidence": rollback_copies != Some(0),
                    "scope": "Active SQLite index only. Backups, snapshots and physical media are excluded."}
            }))
        }
        Command::Export { output } => Ok(json!({
            "schema_version": 1, "exported_passages": store::export(&db, &output)?,
            "scope": "This export contains private evidence. It is separate from the index and survives index deletion."
        })),
        Command::DiscardRollback { confirm } => {
            ensure!(
                confirm,
                "Run discard-rollback --confirm to discard pre-upgrade copies."
            );
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let copies = files::rollback_copies(&state)?;
            for copy in &copies {
                ensure!(
                    copy.symlink_metadata()?.file_type().is_file(),
                    "An unexpected rollback entry needs manual inspection."
                );
            }
            for copy in &copies {
                std::fs::remove_file(copy)
                    .context("Rollback cleanup stopped before removing all copies.")?;
            }
            tx.commit()?;
            Ok(json!({
                "schema_version": 1, "discarded_copies": copies.len(),
                "retained_rollback_copies": files::rollback_copy_count(&state)?,
                "scope": "Local rollback files only. Copies elsewhere and physical media are excluded."
            }))
        }
        #[cfg(feature = "pdf")]
        Command::PdfWorker => unreachable!("The worker is dispatched before opening the index."),
        #[cfg(feature = "xlsx")]
        Command::XlsxWorker => unreachable!("Excel worker is dispatched before opening the index."),
        #[cfg(feature = "docx")]
        Command::DocxWorker => unreachable!("The worker is dispatched before opening the index."),
        #[cfg(feature = "mcp")]
        Command::Mcp { .. } => unreachable!("MCP is dispatched before opening the writable index."),
        Command::Capabilities | Command::Plan { .. } => unreachable!(),
        #[cfg(feature = "dashboard")]
        Command::Health { .. } | Command::Dashboard { .. } | Command::History { .. } => {
            unreachable!("Dashboard commands are dispatched before opening the writable index.")
        }
        #[cfg(feature = "model")]
        Command::Demo => unreachable!(),
        #[cfg(feature = "shared")]
        Command::Serve { .. } | Command::ServiceDoctor { .. } => unreachable!(),
        #[cfg(feature = "inference")]
        Command::ModelManifest | Command::ModelInstall { .. } => unreachable!(),
    }
}

fn explain_query(result: &Value, first_stage: &[Value]) -> Value {
    let mode = result["mode"].as_str().unwrap_or_default();
    let vector = if mode == "lexical" {
        "disabled"
    } else if result["vector_coverage"]["partial"] == true {
        "incomplete"
    } else {
        "ran"
    };
    let graph = &result["graph_coverage"];
    let graph_status = if result["graph"] == false {
        "disabled"
    } else if graph["unavailable"] == true {
        "unavailable"
    } else if graph["partial"] == true {
        "incomplete"
    } else {
        "ran"
    };
    let reranking = &result["reranking"];
    let results: Vec<_> = result["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, hit)| {
            let ranking = &hit["ranking"];
            let signals: Vec<_> = [
                ("lexical", ranking["lexical_score"].is_number()),
                ("vector", ranking["cosine_similarity"].is_number()),
                ("graph", ranking["method"] == "graph_assertion"),
            ]
            .into_iter()
            .filter_map(|(signal, used)| used.then_some(signal))
            .collect();
            json!({
                "chunk_id": hit["chunk_id"],
                "rank": index + 1,
                "rank_before_reranking": first_stage.iter()
                    .position(|id| id == &hit["chunk_id"]).map(|rank| rank + 1),
                "signals": signals,
            })
        })
        .collect();
    json!({
        "stages": {
            "lexical": { "status": if mode == "vector" { "disabled" } else { "ran" } },
            "vector": { "status": vector },
            "graph": { "status": graph_status, "reason": graph["seed_status"] },
            "reranking": {
                "status": reranking["status"].as_str().unwrap_or("disabled"),
                "reason": reranking["reason"],
            },
        },
        "results": results,
    })
}

fn retrieve(
    db: &mut rusqlite::Connection,
    question: &str,
    limit: usize,
    graph: bool,
    mode: store::Mode,
    embedding: &EmbeddingOptions,
) -> Result<Value> {
    if mode == store::Mode::Lexical {
        ensure!(
            !embedding.requested(),
            "An embedding model needs vector or hybrid mode."
        );
        return store::query(db, question, limit, graph, mode, None);
    }
    #[cfg(feature = "model")]
    {
        store::validate_query(question, limit)?;
        store::require_advanced(db)?;
        let endpoint = embedding.open()?;
        vector::require_model(db, endpoint.fingerprint(), endpoint.dimensions())?;
        let values = endpoint.embed(question, true)?;
        let mut result = store::query(
            db,
            question,
            limit,
            graph,
            mode,
            Some(store::VectorQuery {
                fingerprint: endpoint.fingerprint(),
                values: &values,
            }),
        )?;
        result["embedding_input"] = json!(endpoint.input_notice());
        Ok(result)
    }
    #[cfg(not(feature = "model"))]
    anyhow::bail!("Build with the model feature to use vector or hybrid search.")
}

/// Application-provided embedding adapter. A change of model, dimensions, or
/// preprocessing must use a new fingerprint and a fresh index.
#[cfg(feature = "model")]
pub trait Embedder {
    fn fingerprint(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn embed(&self, text: &str, query: bool) -> Result<Vec<f32>>;
    fn input_notice(&self) -> &'static str;
}

#[cfg(feature = "model")]
impl Embedder for model::EmbeddingEndpoint {
    fn fingerprint(&self) -> &str {
        self.fingerprint()
    }
    fn dimensions(&self) -> usize {
        self.dimensions()
    }
    fn embed(&self, text: &str, query: bool) -> Result<Vec<f32>> {
        self.embed(text, query)
    }
    fn input_notice(&self) -> &'static str {
        "Text is sent to the configured embedding endpoint."
    }
}

#[cfg(feature = "inference")]
impl Embedder for inference::NativeEmbedder {
    fn fingerprint(&self) -> &str {
        self.fingerprint()
    }
    fn dimensions(&self) -> usize {
        self.dimensions()
    }
    fn embed(&self, text: &str, query: bool) -> Result<Vec<f32>> {
        self.embed(text, query)
    }
    fn input_notice(&self) -> &'static str {
        "The embedding model processes text in this process. Check the separate consumer and answer model."
    }
}

#[cfg(feature = "model")]
fn index_vectors(
    db: &mut rusqlite::Connection,
    endpoint: &dyn Embedder,
    limit: usize,
    retry: bool,
) -> Result<Value> {
    let fingerprint = endpoint.fingerprint();
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    vector::configure(&tx, fingerprint, endpoint.dimensions())?;
    tx.commit()?;
    let mut report = run_enrichment(
        db,
        enrichment::Lane::Vector,
        fingerprint,
        limit,
        retry,
        |tx, id| vector::unfinished_text(tx, id, fingerprint),
        |text| endpoint.embed(&text, false).map(Some),
        |tx, id, values| vector::publish(tx, id, fingerprint, &values),
    )?;
    report["model_input"] = json!(endpoint.input_notice());
    Ok(report)
}

#[cfg(feature = "model")]
fn extract_graph(
    db: &mut rusqlite::Connection,
    endpoint: &model::ModelEndpoint,
    limit: usize,
    retry: bool,
    source_clauses: bool,
) -> Result<Value> {
    let mut root = None;
    let clause_fingerprint;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let fingerprint = if source_clauses {
        root = Some(graph::local_text_root(&tx)?);
        clause_fingerprint = format!(
            "{}{}",
            graph::LOCAL_REVIEW_FINGERPRINT_PREFIX,
            endpoint.source_clause_fingerprint()?
        );
        graph::install_review_queue(&tx)?;
        clause_fingerprint.as_str()
    } else {
        endpoint.fingerprint()
    };
    graph::configure(&tx, fingerprint)?;
    tx.commit()?;
    let publish = |tx: &rusqlite::Connection, id: &str, assertions: Vec<Value>| {
        let save = if source_clauses {
            graph::stage
        } else {
            graph::publish
        };
        save(tx, id, fingerprint, endpoint.predicates(), &assertions)
    };
    let mut report = if let Some(root) = root {
        let mut remaining = files::MAX_BATCH_BYTES;
        run_enrichment(
            db,
            enrichment::Lane::Graph,
            fingerprint,
            limit,
            retry,
            |tx, id| graph::unfinished_source(tx, id, fingerprint),
            |source| {
                let Ok(document) = source.read_current(&root, &mut remaining) else {
                    return Ok(None);
                };
                if document.chars().count() > 12_000 {
                    return Ok(None);
                }
                endpoint
                    .extract_source_clauses(&document, &source.text)
                    .map(Some)
            },
            publish,
        )?
    } else {
        run_enrichment(
            db,
            enrichment::Lane::Graph,
            fingerprint,
            limit,
            retry,
            |tx, id| graph::unfinished_text(tx, id, fingerprint),
            |text| endpoint.extract(&text).map(Some),
            publish,
        )?
    };
    report["model_input"] = if source_clauses {
        json!("bounded current document text may go to the configured loopback verifier")
    } else {
        json!("selected passage text may go to the configured endpoint")
    };
    if source_clauses {
        report["relationships"] = json!("pending_confirmation");
    }
    Ok(report)
}

#[cfg(feature = "model")]
fn run_enrichment<I, T>(
    db: &mut rusqlite::Connection,
    lane: enrichment::Lane,
    fingerprint: &str,
    limit: usize,
    retry: bool,
    prepare: impl Fn(&rusqlite::Connection, &str) -> Result<Option<I>>,
    mut request: impl FnMut(I) -> Result<Option<T>>,
    publish: impl Fn(&rusqlite::Connection, &str, T) -> Result<bool>,
) -> Result<Value> {
    use enrichment::{claim, completed, failed, progress, ready, release};
    use model::Failure;
    use rusqlite::TransactionBehavior::Immediate;

    let tx = db.transaction_with_behavior(Immediate)?;
    if retry {
        enrichment::retry(&tx, lane, fingerprint)?;
    }
    let pending = ready(&tx, lane, fingerprint, limit)?;
    tx.commit()?;

    let mut completed_chunks = 0;
    let mut stale = 0;
    let mut failures = Vec::new();
    let mut provider = None;
    for chunk_id in pending {
        let tx = db.transaction_with_behavior(Immediate)?;
        let Some(work) = claim(&tx, lane, fingerprint, &chunk_id)? else {
            stale += 1;
            continue;
        };
        let Some(input) = prepare(&tx, &chunk_id)? else {
            stale += 1;
            continue;
        };
        tx.commit()?;
        let result = request(input);

        let tx = db.transaction_with_behavior(Immediate)?;
        if !work.current(&tx)? {
            stale += 1;
            continue;
        }
        let kind = match result {
            Ok(None) => {
                release(&tx, &work)?;
                tx.commit()?;
                failures.push(json!({"chunk_id": chunk_id, "code": "source_unavailable",
                    "message": "Check the local source and run ingest before retrying. Source-clause verification accepts up to 12000 characters."}));
                continue;
            }
            Ok(Some(value)) => {
                let mut tx = tx;
                let savepoint = tx.savepoint()?;
                match publish(&savepoint, &chunk_id, value) {
                    Ok(true) => {
                        ensure!(
                            completed(&savepoint, &work)?,
                            "Enrichment lease expired. Retry."
                        );
                        savepoint.commit()?;
                        tx.commit()?;
                        completed_chunks += 1;
                        continue;
                    }
                    Ok(false) => {
                        stale += 1;
                        continue;
                    }
                    Err(error) if error.is::<rusqlite::Error>() => return Err(error),
                    Err(_) => {}
                }
                drop(savepoint);
                failed(&tx, &work)?;
                tx.commit()?;
                failures.push(json!({"chunk_id": chunk_id, "code": Failure::Item.code()}));
                continue;
            }
            Err(error) => Failure::of(&error),
        };
        if kind == Failure::Item {
            failed(&tx, &work)?;
        } else {
            release(&tx, &work)?;
            if let Failure::RateLimited(seconds) = kind {
                enrichment::pause(&tx, lane, fingerprint, seconds)?;
            }
            provider = Some(kind.code());
        }
        tx.commit()?;
        failures.push(json!({"chunk_id": chunk_id, "code": kind.code()}));
        if provider.is_some() {
            break;
        }
    }

    let progress = progress(db, lane, fingerprint)?;
    Ok(json!({
        "schema_version": 1, "completed_chunks": completed_chunks, "stale_chunks": stale,
        "failures": failures, "pending": progress["pending"].as_i64().unwrap_or(0) > 0,
        "progress": progress, "provider_status": provider,
    }))
}

pub fn run_cli() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(feature = "mcp")]
    if let Command::Mcp {
        #[cfg(feature = "inference")]
        ref reranker_profile,
    } = cli.command
    {
        return match cli
            .state_dir
            .as_deref()
            .ok_or("MCP requires an explicit --state-dir for an existing personal index.")
            .and_then(|state| {
                mcp::serve(
                    state,
                    #[cfg(feature = "inference")]
                    reranker_profile.as_deref(),
                )
            }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("{message}");
                ExitCode::FAILURE
            }
        };
    }
    #[cfg(feature = "xlsx")]
    if matches!(cli.command, Command::XlsxWorker) {
        return if xlsx::worker().is_ok() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        };
    }
    #[cfg(feature = "docx")]
    if matches!(cli.command, Command::DocxWorker) {
        return if docx::worker().is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    #[cfg(feature = "pdf")]
    if matches!(cli.command, Command::PdfWorker) {
        return if pdf::worker().is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let json = cli.json;
    #[cfg(feature = "model")]
    let answer_command = matches!(cli.command, Command::Answer { .. });
    #[cfg(not(feature = "model"))]
    let answer_command = false;
    let require_complete = matches!(
        cli.command,
        Command::Ingest {
            require_complete: true,
            ..
        }
    );
    let (result, failed) = match run(cli) {
        // A withheld answer prints its full report and exits nonzero.
        Ok(result) => {
            let withheld = answer_command && result["status"] == "unavailable";
            let incomplete = require_complete
                && (result["inventory_complete"] != true
                    || result["sync_blocked"] != false
                    || result["deferred_sources"].as_u64() != Some(0)
                    || result["failures"]
                        .as_array()
                        .is_none_or(|failures| !failures.is_empty()));
            if incomplete && !json {
                eprintln!("Folder sync is incomplete. Review the ingest report before retrying.");
            }
            (result, withheld || incomplete)
        }
        Err(error) if !json => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            #[cfg_attr(not(feature = "model"), allow(unused_mut))]
            let mut failure =
                json!({"schema_version": 1, "status": "error", "message": error.to_string()});
            #[cfg(feature = "model")]
            if let Some(answer) = error.downcast_ref::<AnswerFailure>() {
                failure["delivery"] = answer.delivery.clone();
            }
            (failure, true)
        }
    };
    let mut stdout = io::stdout().lock();
    let written = if json {
        serde_json::to_writer(&mut stdout, &result)
    } else {
        serde_json::to_writer_pretty(&mut stdout, &result)
    };
    if written.is_err() || writeln!(stdout).is_err() || failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
