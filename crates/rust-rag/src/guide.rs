use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use serde_json::{Value, json};
use std::path::Path;

pub fn advanced_profile(path: &Path) -> Result<bool> {
    let bytes = crate::files::read(path, &mut (8 * 1024))?;
    let value: Value =
        serde_json::from_slice(&bytes).context("Expected a local JSON index profile.")?;
    let object = value
        .as_object()
        .context("Expected a local JSON index profile.")?;
    ensure!(
        object.len() == 2 && object.get("schema_version") == Some(&json!(1)),
        "Expected schema_version 1 and index (text or advanced)."
    );
    match object.get("index").and_then(Value::as_str) {
        Some("text") => Ok(false),
        Some("advanced") => Ok(true),
        _ => anyhow::bail!("Choose a text or advanced index."),
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Profile {
    Personal,
    Obsidian,
    Semantic,
    Graph,
    Shared,
    Company,
    Aws,
    Custom,
}

pub fn capabilities() -> Value {
    json!({
        "schema_version": 1, "product": "Rust RAG",
        "version": env!("CARGO_PKG_VERSION"),
        "compiled": {
            "text": true, "pdf": cfg!(feature = "pdf"),
            "ocr": cfg!(all(feature = "ocr", target_os = "linux")),
            "docx": cfg!(feature = "docx"),
            "xlsx": cfg!(feature = "xlsx"),
            "model_endpoint": cfg!(feature = "model"),
            "native_embeddings": cfg!(feature = "inference"),
            "mcp_stdio": cfg!(feature = "mcp"),
            "health_report": cfg!(feature = "dashboard"),
            "local_dashboard": cfg!(feature = "dashboard"),
            "shared_service": cfg!(feature = "shared"),
            "aws_adapters": cfg!(feature = "aws")
        },
        "qualification": "See status.md for verification of each deployment."
    })
}

pub fn plan(profile: Option<Profile>) -> Value {
    let (name, reason, features, next): (&str, &str, &[&str], &[&str]) = match profile {
        None => (
            "choose",
            "Start with the kind of question you need answered, who can see the sources, and where data may go.",
            &[],
            &[
                "rust-rag plan --help",
                "Read docs/field-guide.md#decision-tree",
            ],
        ),
        Some(Profile::Personal) => (
            "personal",
            "One owner needs passages from Markdown or text and can start with exact terms.",
            &[],
            &[
                "rust-rag init",
                "rust-rag doctor",
                "rust-rag ingest <folder>",
                "rust-rag query <question>",
            ],
        ),
        Some(Profile::Obsidian) => (
            "obsidian",
            "Search the notes on disk, skipping Obsidian configuration and trash. Use a separate state folder for each vault.",
            &[],
            &[
                "rust-rag --state-dir <private-state> init",
                "rust-rag --state-dir <private-state> ingest <vault> --sync",
            ],
        ),
        Some(Profile::Semantic) => (
            "semantic",
            "Words in the question and sources differ. Add meaning-based ranking with an explicit local model or embedding API.",
            &["inference or model"],
            &[
                "rust-rag init --upgrade",
                "rust-rag ingest <folder>",
                "Read crates/rust-rag/README.md",
            ],
        ),
        Some(Profile::Graph) => (
            "graph",
            "Questions depend on relationships across documents. Start with typed, source-anchored edges and bounded traversal.",
            &["model"],
            &[
                "rust-rag init --upgrade",
                "rust-rag ingest <folder>",
                "rust-rag extract-graph --model-endpoint <trusted-config>",
                "rust-rag query <question> --graph",
            ],
        ),
        Some(Profile::Shared) => (
            "shared",
            "Multiple users need scoped access. Choose identity, collections, grants, egress rules and recovery before serving traffic.",
            &["shared"],
            &[
                "Read docs/setup.md#rehearse-private-graph-rag-with-synthetic-sources",
                "Read infra/private-vm.md",
                "rust-rag service-doctor --profile <profile>",
                "rust-rag serve --profile <profile>",
            ],
        ),
        Some(Profile::Company) => (
            "company",
            "Teams need typed Graph RAG with collection grants, private model access, and repeatable recovery. Start with the portable shared service and choose a deployment separately.",
            &["shared"],
            &[
                "Read docs/setup.md#rehearse-private-graph-rag-with-synthetic-sources",
                "Read infra/private-vm.md",
                "Read infra/README.md#deployment-targets",
                "rust-rag service-doctor --profile <profile>",
                "rust-rag serve --profile <profile>",
            ],
        ),
        Some(Profile::Aws) => (
            "aws",
            "Use the shared service with versioned S3 inputs and backups. Add Neptune only when your graph needs it.",
            &["aws"],
            &[
                "Read infra/aws-deployment.md",
                "rust-rag service-doctor --profile <profile>",
            ],
        ),
        Some(Profile::Custom) => (
            "custom",
            "Choose source, retrieval, models, storage, access, and hosting independently. Integrate any missing adapters at those boundaries.",
            &[],
            &[
                "Read docs/field-guide.md",
                "Read docs/rag-reference.md",
                "rust-rag capabilities",
            ],
        ),
    };
    json!({
        "schema_version": 1, "profile": name, "why": reason,
        "required_features": features, "next": next,
        "evaluation_required": false, "capabilities": capabilities(),
        "privacy": "A hosted harness may receive returned passages and metadata. Check model destinations and access before processing private data."
    })
}
