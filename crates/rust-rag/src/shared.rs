//! Single-organization service. Each collection owns its SQLite index so a
//! denied collection cannot affect retrieval statistics or graph traversal.

#[path = "shared_auth.rs"]
mod auth;
#[path = "shared_backup.rs"]
mod backup;
#[path = "shared_graph_policy.rs"]
mod graph_policy;
#[path = "shared_relationship.rs"]
mod relationship;
#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;

use self::{
    auth::{Authenticator, Identity, Principal, strict_json, subject},
    backup::BackupStore,
    graph_policy::{Catalog, PublicationPolicy},
};
use crate::{
    enrichment::{self, Lane},
    files, graph,
    model::{EmbeddingEndpoint, Failure, ModelEndpoint},
    store::{self, Mode, VectorQuery},
    vector,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior::Immediate, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError, mpsc},
    thread,
    time::{Duration, Instant},
};

const MAX_POLICY: usize = 256 * 1024;
const MAX_BODY: usize = 32 * 1024;
const MAX_COLLECTIONS: usize = 64;
const POLICY_ERROR: ApiError = ApiError::new(
    503,
    "POLICY_UNAVAILABLE",
    "The current access policy could not be verified.",
);
const WRITER_BUSY: ApiError = ApiError::new(
    409,
    "WRITER_BUSY",
    "A shared maintenance operation is in progress. Retry shortly.",
)
.retry_after(1);
const ACCESS_CHANGED: ApiError = ApiError::new(
    409,
    "ACCESS_CHANGED",
    "Access changed while processing this request. Retry with the current policy.",
);
const DESTINATION_DENIED: ApiError = ApiError::new(
    403,
    "MODEL_DESTINATION_DENIED",
    "The selected model is not approved for this scope.",
);
const NOT_FOUND: ApiError = ApiError::new(
    404,
    "EVIDENCE_NOT_FOUND",
    "No readable evidence matches that identifier.",
);
const REVIEW_DENIED: ApiError = ApiError::new(
    403,
    "GRAPH_REVIEW_DENIED",
    "Graph review requires current read and review grants for this collection.",
);
const REVIEW_STALE: ApiError = ApiError::new(
    409,
    "GRAPH_REVIEW_STALE",
    "Sync the collection if needed and reload the review queue.",
);
const REVIEW_INVALID: ApiError = ApiError::new(
    400,
    "GRAPH_REVIEW_REQUEST",
    "Supply a valid collection, proposal, revision and decision.",
);
const REVIEW_FINGERPRINT_PREFIX: &str = "shared-review-v1:";
const DECLARATION_STATE: &str = "
CREATE TABLE IF NOT EXISTS shared_declaration_state (
    id INTEGER PRIMARY KEY CHECK (id=1),
    catalog_digest TEXT
);
CREATE TABLE IF NOT EXISTS shared_declaration_chunks (
    chunk_id TEXT PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL REFERENCES graph_config(fingerprint) ON DELETE CASCADE,
    digest TEXT NOT NULL
);";

#[derive(Clone, Copy, Debug)]
struct ApiError(u16, &'static str, &'static str, Option<u64>);
impl ApiError {
    const fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self(status, code, message, None)
    }

    const fn retry_after(mut self, seconds: u64) -> Self {
        self.3 = Some(seconds);
        self
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn unavailable(_: impl std::fmt::Debug) -> ApiError {
    ApiError::new(
        503,
        "REQUEST_FAILED",
        "The requested operation could not be completed safely.",
    )
}

fn model_error(failure: Failure) -> ApiError {
    match failure {
        Failure::Item => ApiError::new(
            502,
            "MODEL_RESPONSE_INVALID",
            "The model returned an invalid response.",
        ),
        Failure::Provider => ApiError::new(
            503,
            "MODEL_PROVIDER_UNAVAILABLE",
            "The model provider is unavailable. Retry shortly.",
        )
        .retry_after(1),
        Failure::RateLimited(seconds) => ApiError::new(
            503,
            "MODEL_RATE_LIMITED",
            "The model provider is rate limited. Retry shortly.",
        )
        .retry_after(seconds),
    }
}

fn graph_review_error(error: anyhow::Error) -> ApiError {
    match error.downcast_ref::<graph::SharedReviewError>() {
        Some(graph::SharedReviewError::Invalid) => REVIEW_INVALID,
        Some(graph::SharedReviewError::Stale) => REVIEW_STALE,
        Some(graph::SharedReviewError::Conflict) => ApiError::new(
            409,
            "GRAPH_REVIEW_CONFLICT",
            "The proposal or idempotency key already has a different decision.",
        ),
        None => unavailable(error),
    }
}

fn bounded_file(path: &Path, mut limit: usize) -> Result<Vec<u8>> {
    files::read(path, &mut limit)
}

fn clear_declaration_chunk(db: &Connection, chunk: &str) -> Result<()> {
    db.execute(
        "DELETE FROM shared_declaration_chunks WHERE chunk_id=?1",
        [chunk],
    )?;
    db.execute(
        "DELETE FROM graph_chunks WHERE chunk_row IN (SELECT row_id FROM chunks WHERE chunk_id=?1)",
        [chunk],
    )?;
    db.execute(
        "DELETE FROM enrichment_attempts WHERE lane='graph' AND chunk_id=?1",
        [chunk],
    )?;
    Ok(())
}

fn label(value: &str) -> bool {
    (1..=64).contains(&value.len()) && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
        // These names alias special device files on Windows even with an extension.
        && !matches!(value, "con" | "prn" | "aux" | "nul" | "com1" | "com2" | "com3" | "com4" | "com5" | "com6" | "com7" | "com8" | "com9" | "lpt1" | "lpt2" | "lpt3" | "lpt4" | "lpt5" | "lpt6" | "lpt7" | "lpt8" | "lpt9")
}

fn hex_id(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn overlap(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    let (left, right) = (
        PathBuf::from(left.to_string_lossy().to_lowercase()),
        PathBuf::from(right.to_string_lossy().to_lowercase()),
    );
    left.starts_with(&right) || right.starts_with(&left)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema_version: u32,
    state_dir: PathBuf,
    identity: Identity,
    policy: Value,
    backup: Option<Value>,
    sources: BTreeMap<String, Value>,
    #[serde(default)]
    pdf: bool,
    ocr_profile: Option<PathBuf>,
    #[serde(default)]
    allow_proxy_http: bool,
    #[serde(default)]
    endpoints: BTreeMap<String, PathBuf>,
    embedding: Option<EmbeddingSpec>,
    reranker: Option<Value>,
    #[serde(default)]
    graph: GraphSpec,
}

#[derive(Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
enum EmbeddingSpec {
    Endpoint {
        name: String,
        config: PathBuf,
    },
    Hash {
        model: String,
        dimensions: usize,
    },
    #[cfg(feature = "inference")]
    Native {
        cache_dir: PathBuf,
        runtime_library: PathBuf,
        runtime_sha256: String,
    },
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphSpec {
    #[serde(default)]
    mode: GraphMode,
    extractor: Option<String>,
    projection: Option<Value>,
    #[serde(default)]
    allow_unreviewed_model_links: bool,
}

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GraphMode {
    #[default]
    Off,
    Typed,
    Neptune,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectorySpec {
    kind: String,
    directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderSpec {
    kind: String,
    root: PathBuf,
}

enum PolicySource {
    File(PathBuf),
    #[cfg(feature = "aws")]
    S3(crate::cloud::S3Policy),
}

impl PolicySource {
    fn read(&self) -> Result<(Policy, String)> {
        let (raw, policy) = match self {
            Self::File(root) => {
                let current = root.join("current.json");
                let raw = bounded_file(&current, MAX_POLICY)?;
                let policy: Policy = serde_json::from_value(strict_json(&raw)?)?;
                ensure!(policy.version > 0, "Invalid policy version.");
                let versions = root.join("versions");
                let archive = versions.join(format!("{}.json", policy.version));
                ensure!(
                    bounded_file(&archive, MAX_POLICY)? == raw
                        && bounded_file(&current, MAX_POLICY)? == raw,
                    "Publish the current policy and its independent version consistently."
                );
                (raw, policy)
            }
            #[cfg(feature = "aws")]
            Self::S3(source) => {
                let raw = source.read()?;
                ensure!(raw.len() <= MAX_POLICY, "Policy exceeds its size limit.");
                let policy = serde_json::from_value(strict_json(&raw)?)?;
                (raw, policy)
            }
        };
        Ok((policy, files::digest(&raw)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    version: i64,
    issuer: String,
    administrators: Vec<String>,
    subjects: BTreeMap<String, Vec<String>>,
    collections: BTreeMap<String, Grant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    endpoints: Vec<String>,
    #[serde(default)]
    graph_review_subjects: Vec<String>,
    #[serde(default)]
    graph_review_roles: Vec<String>,
    graph_publication: Option<PublicationPolicy>,
    #[serde(default)]
    reviewed_relationships: Vec<relationship::ReviewedRelationship>,
}

impl Policy {
    fn scope(&self, principal: &Principal) -> ApiResult<Vec<String>> {
        principal.ensure_current()?;
        let roles = self.subjects.get(&principal.subject);
        Ok(self
            .collections
            .iter()
            .filter(|(_, grant)| {
                grant.subjects.contains(&principal.subject)
                    || roles
                        .is_some_and(|roles| grant.roles.iter().any(|role| roles.contains(role)))
            })
            .map(|(name, _)| name.clone())
            .collect())
    }

    fn admin(&self, principal: &Principal) -> ApiResult<()> {
        principal.ensure_current()?;
        if !self.administrators.contains(&principal.subject) {
            return Err(ApiError::new(
                403,
                "ADMIN_REQUIRED",
                "This operation requires a current administrator grant.",
            ));
        }
        Ok(())
    }

    fn destination(&self, collection: &str, endpoint: &str) -> ApiResult<()> {
        if !self
            .collections
            .get(collection)
            .is_some_and(|grant| grant.endpoints.iter().any(|e| e == endpoint))
        {
            return Err(DESTINATION_DENIED);
        }
        Ok(())
    }

    fn graph_reviewer(&self, principal: &Principal, collection: &str) -> ApiResult<&Grant> {
        let grant = self.collections.get(collection).ok_or(REVIEW_DENIED)?;
        if !self.scope(principal)?.iter().any(|name| name == collection)
            || !(grant.graph_review_subjects.contains(&principal.subject)
                || self.subjects.get(&principal.subject).is_some_and(|roles| {
                    grant
                        .graph_review_roles
                        .iter()
                        .any(|role| roles.contains(role))
                }))
        {
            return Err(REVIEW_DENIED);
        }
        Ok(grant)
    }
}

enum Source {
    Folder(PathBuf),
    #[cfg(feature = "aws")]
    S3(crate::cloud::S3Source),
}

impl Source {
    fn folder(&self) -> Option<&Path> {
        match self {
            Self::Folder(root) => Some(root),
            #[cfg(feature = "aws")]
            Self::S3(_) => None,
        }
    }

    fn identity(&self) -> Result<String> {
        match self {
            Self::Folder(root) => Ok(files::display_path(root)?.to_owned()),
            #[cfg(feature = "aws")]
            Self::S3(source) => Ok(source.identity()),
        }
    }

    fn ingest(
        &self,
        db: &mut Connection,
        pdf: bool,
        ocr: &mut store::OcrBatch<'_>,
    ) -> Result<Value> {
        match self {
            Self::Folder(root) => {
                ensure!(
                    files::directory(root)? == *root,
                    "The trusted source root changed."
                );
                let before = files::inventory(root)?;
                let mut report =
                    store::ingest_with_ocr(db, root, true, pdf, cfg!(feature = "docx"), ocr)?;
                let after = files::inventory(root)?;
                if before.files != after.files
                    || before.entries != after.entries
                    || before.skipped_links != after.skipped_links
                {
                    report["sync_blocked"] = json!(true);
                }
                Ok(report)
            }
            #[cfg(feature = "aws")]
            Self::S3(source) => store::ingest_s3(db, source, pdf),
        }
    }

    fn cite(&self, collection: &str, item: &mut Value) -> Result<()> {
        let path = item["path"]
            .as_str()
            .context("Evidence lacks a source path.")?;
        match self {
            Self::Folder(root) => {
                let relative = Path::new(path).strip_prefix(files::display_path(root)?)?;
                item["path"] = json!(files::display_path(relative)?.replace('\\', "/"));
            }
            #[cfg(feature = "aws")]
            Self::S3(_) => {}
        }
        item["collection"] = json!(collection);
        Ok(())
    }
}

enum Embedding {
    Endpoint {
        name: String,
        endpoint: Box<EmbeddingEndpoint>,
    },
    Hash {
        fingerprint: String,
        dimensions: usize,
    },
    #[cfg(feature = "inference")]
    Native(Box<crate::inference::NativeEmbedder>),
}

impl Embedding {
    fn fingerprint(&self) -> &str {
        match self {
            Self::Endpoint { endpoint, .. } => endpoint.fingerprint(),
            Self::Hash { fingerprint, .. } => fingerprint,
            #[cfg(feature = "inference")]
            Self::Native(embedding) => embedding.fingerprint(),
        }
    }

    fn dimensions(&self) -> usize {
        match self {
            Self::Endpoint { endpoint, .. } => endpoint.dimensions(),
            Self::Hash { dimensions, .. } => *dimensions,
            #[cfg(feature = "inference")]
            Self::Native(embedding) => embedding.dimensions(),
        }
    }

    fn destination(&self) -> Option<&str> {
        match self {
            Self::Endpoint { name, .. } => Some(name),
            _ => None,
        }
    }

    fn embed(&self, text: &str, query: bool) -> Result<Vec<f32>> {
        match self {
            Self::Endpoint { endpoint, .. } => endpoint.embed(text, query),
            #[cfg(feature = "inference")]
            Self::Native(embedding) => embedding.embed(text, query),
            Self::Hash { dimensions, .. } => {
                // This download-free option hashes words. It is a lexical baseline,
                // not a semantic model or a provider-compatible vector format.
                let mut values = vec![0.0_f32; *dimensions];
                for word in text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|s| !s.is_empty())
                {
                    use sha2::{Digest as _, Sha256};
                    let hash = Sha256::digest(word.to_lowercase().as_bytes());
                    let index = u32::from_le_bytes(hash[..4].try_into()?) as usize % dimensions;
                    values[index] += if hash[4] & 1 == 0 { 1.0 } else { -1.0 };
                }
                if values.iter().all(|v| *v == 0.0) {
                    values[0] = 1.0;
                }
                Ok(values)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ConfiguredLane<'a> {
    Graph(&'a str, &'a ModelEndpoint),
    Vector(&'a Embedding),
}

impl<'a> ConfiguredLane<'a> {
    fn key_with_review(self, review: Option<&'a str>) -> (Lane, &'a str) {
        match self {
            Self::Graph(_, endpoint) => (Lane::Graph, review.unwrap_or(endpoint.fingerprint())),
            Self::Vector(embedding) => (Lane::Vector, embedding.fingerprint()),
        }
    }

    fn destination(self) -> Option<&'a str> {
        match self {
            Self::Graph(name, _) => Some(name),
            Self::Vector(embedding) => embedding.destination(),
        }
    }
}

enum PreparedEnrichment {
    Graph(Vec<Value>),
    Vector(Vec<f32>),
}

struct Service {
    auth: Authenticator,
    policy_source: PolicySource,
    meta: Mutex<Connection>,
    gate: RwLock<()>,
    mutation: Mutex<()>,
    job: Mutex<Value>,
    state: PathBuf,
    binding: String,
    sources: BTreeMap<String, Source>,
    // Keep WAL sidecars open between requests. Closing the last connection
    // can briefly exclude new opens while SQLite cleans up the sidecars.
    _collection_connections: Mutex<Vec<Connection>>,
    _lease: File,
    backups: Option<BackupStore>,
    pdf: bool,
    ocr: Option<crate::ocr::Worker>,
    allow_proxy_http: bool,
    endpoints: BTreeMap<String, ModelEndpoint>,
    embedding: Option<Embedding>,
    #[cfg(feature = "inference")]
    reranker: Option<crate::inference::Reranker>,
    reranker_status: Option<&'static str>,
    graph: GraphMode,
    extractor: Option<String>,
    allow_unreviewed_model_links: bool,
    #[cfg(feature = "aws")]
    projection: Option<crate::cloud::NeptuneGraph>,
}

struct ServiceWriter<'a> {
    visibility: Option<RwLockWriteGuard<'a, ()>>,
    gate: &'a RwLock<()>,
    _reservation: MutexGuard<'a, ()>,
}

impl ServiceWriter<'_> {
    fn allow_reads<T>(&mut self, action: impl FnOnce() -> T) -> ApiResult<T> {
        drop(self.visibility.take());
        let result = action();
        self.visibility = Some(self.gate.write().map_err(unavailable)?);
        Ok(result)
    }
}

fn directory_spec(value: &Value, create: bool) -> Result<PathBuf> {
    let spec: DirectorySpec = serde_json::from_value(value.clone())?;
    ensure!(
        spec.kind == "file" && spec.directory.is_absolute(),
        "Use an absolute private directory."
    );
    files::state_directory(&spec.directory, create)
}

fn new_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

impl Service {
    fn permitted_lanes(&self, grant: &Grant) -> [Option<ConfiguredLane<'_>>; 2] {
        [
            self.extractor
                .as_ref()
                .filter(|name| grant.endpoints.contains(name))
                .map(|name| ConfiguredLane::Graph(name, &self.endpoints[name])),
            self.embedding
                .as_ref()
                .filter(|embedding| {
                    embedding.destination().is_none_or(|destination| {
                        grant.endpoints.iter().any(|name| name == destination)
                    })
                })
                .map(ConfiguredLane::Vector),
        ]
    }

    fn open(path: &Path) -> Result<Self> {
        let value = strict_json(&bounded_file(path, MAX_POLICY)?)?;
        let profile: Profile = serde_json::from_value(value.clone())?;
        ensure!(
            profile.schema_version == 1
                && profile.state_dir.is_absolute()
                && (1..=MAX_COLLECTIONS).contains(&profile.sources.len())
                && profile.endpoints.len() <= 32,
            "Use a supported shared profile with a private absolute state directory and 1–64 collections."
        );
        ensure!(
            !profile.pdf || cfg!(feature = "pdf"),
            "Build with the pdf feature to enable shared PDF ingestion."
        );
        let ocr = if let Some(path) = &profile.ocr_profile {
            ensure!(
                path.is_absolute()
                    && !profile.pdf
                    && profile.graph.mode != GraphMode::Neptune
                    && profile
                        .sources
                        .values()
                        .all(|source| source["kind"] == "folder"),
                "Use an absolute OCR profile for folder sources with no separate PDF reader or Neptune projection."
            );
            Some(crate::ocr::Worker::load(path)?)
        } else {
            None
        };
        let auth = Authenticator::new(profile.identity)?;
        let mut protected = Vec::new();
        let policy_source = match profile.policy["kind"].as_str() {
            Some("file") => {
                let dir = directory_spec(&profile.policy, false)?;
                protected.push(dir.clone());
                PolicySource::File(dir)
            }
            #[cfg(feature = "aws")]
            Some("s3") => PolicySource::S3(crate::cloud::S3Policy::new(&profile.policy)?),
            _ => anyhow::bail!("Use a file policy directory, or build with aws for S3."),
        };
        let backups = profile
            .backup
            .map(|config| -> Result<_> {
                match config["kind"].as_str() {
                    Some("file") => {
                        let dir = directory_spec(&config, false)?;
                        protected.push(dir.clone());
                        Ok(BackupStore::File(dir))
                    }
                    #[cfg(feature = "aws")]
                    Some("s3") => Ok(BackupStore::S3(crate::cloud::S3Backups::new(&config)?)),
                    _ => anyhow::bail!(
                        "Use a private file backup directory, or build with aws for S3."
                    ),
                }
            })
            .transpose()?;
        let mut sources = BTreeMap::new();
        let mut roots = Vec::new();
        for (name, config) in profile.sources {
            ensure!(
                label(&name),
                "Use distinct lowercase collection identifiers."
            );
            let source = match config["kind"].as_str() {
                Some("folder") => {
                    let spec: FolderSpec = serde_json::from_value(config)?;
                    ensure!(
                        spec.kind == "folder" && spec.root.is_absolute(),
                        "Use an absolute folder source root."
                    );
                    let dir = files::directory(&spec.root)?;
                    roots.push(dir.clone());
                    Source::Folder(dir)
                }
                #[cfg(feature = "aws")]
                Some("s3") => Source::S3(crate::cloud::S3Source::new(&config)?),
                _ => anyhow::bail!("Use a trusted folder source, or build with aws for S3."),
            };
            sources.insert(name, source);
        }
        let mut endpoints = BTreeMap::new();
        for (name, path) in profile.endpoints {
            ensure!(
                label(&name) && path.is_absolute(),
                "Endpoint names must be lowercase and config paths absolute."
            );
            endpoints.insert(name, ModelEndpoint::open(&path)?);
        }
        let embedding = profile
            .embedding
            .map(|spec| -> Result<_> {
                match spec {
                    EmbeddingSpec::Endpoint { name, config } => {
                        ensure!(
                            label(&name) && !endpoints.contains_key(&name) && config.is_absolute(),
                            "Use a distinct name and absolute embedding endpoint config path."
                        );
                        Ok(Embedding::Endpoint {
                            name,
                            endpoint: Box::new(EmbeddingEndpoint::open(&config)?),
                        })
                    }
                    EmbeddingSpec::Hash { model, dimensions } => {
                        ensure!(
                            model == "lexical-hash-v1" && (16..=2048).contains(&dimensions),
                            "The hash baseline needs lexical-hash-v1 and 16–2048 dimensions."
                        );
                        Ok(Embedding::Hash {
                            fingerprint: format!("rust-shared-lexical-hash-v1-{dimensions}"),
                            dimensions,
                        })
                    }
                    #[cfg(feature = "inference")]
                    EmbeddingSpec::Native {
                        cache_dir,
                        runtime_library,
                        runtime_sha256,
                    } => Ok(Embedding::Native(Box::new(
                        crate::inference::NativeEmbedder::open(
                            &cache_dir,
                            &runtime_library,
                            &runtime_sha256,
                        )?,
                    ))),
                }
            })
            .transpose()?;
        let reranker_requested = profile.reranker.is_some();
        #[cfg(feature = "inference")]
        let reranker = profile
            .reranker
            .map(serde_json::from_value::<crate::inference::RerankerConfig>)
            .transpose()
            .context("Invalid local reranker profile.")?
            .and_then(|config| crate::inference::Reranker::open(&config).ok());
        #[cfg(feature = "inference")]
        let reranker_status = reranker_requested.then_some(if reranker.is_some() {
            "ready"
        } else {
            "unavailable"
        });
        #[cfg(not(feature = "inference"))]
        let reranker_status = reranker_requested.then_some("inference_feature_required");
        if let Some(extractor) = &profile.graph.extractor {
            ensure!(
                profile.graph.mode != GraphMode::Off && endpoints.contains_key(extractor),
                "Choose a configured chat endpoint for graph extraction."
            );
            ensure!(
                profile.graph.allow_unreviewed_model_links
                    || (profile.graph.mode == GraphMode::Typed
                        && sources.values().all(|source| source.folder().is_some())),
                "Model extraction with Neptune or non-folder sources requires explicit allow_unreviewed_model_links opt-in."
            );
        }
        ensure!(
            profile.graph.mode != GraphMode::Neptune || cfg!(feature = "aws"),
            "Build with aws to enable Neptune."
        );
        ensure!(
            profile.graph.projection.is_some() == (profile.graph.mode == GraphMode::Neptune),
            "Configure projection only for graph mode neptune."
        );
        #[cfg(feature = "aws")]
        let projection = profile
            .graph
            .projection
            .as_ref()
            .map(crate::cloud::NeptuneGraph::new)
            .transpose()?;

        let state = files::state_directory(&profile.state_dir, true)?;
        protected.push(state.clone());
        roots.extend(protected);
        for (index, root) in roots.iter().enumerate() {
            ensure!(
                roots[..index].iter().all(|other| !overlap(root, other)),
                "Keep source, state, policy and backup directories disjoint."
            );
        }
        let identities: BTreeMap<_, _> = sources
            .iter()
            .map(|(key, source)| Ok((key, source.identity()?)))
            .collect::<Result<_>>()?;
        for (index, identity) in identities.values().enumerate() {
            if identity.starts_with("s3://") {
                ensure!(
                    identities
                        .values()
                        .take(index)
                        .all(|other| !identity.starts_with(other) && !other.starts_with(identity)),
                    "S3 collection prefixes must not overlap."
                );
            }
        }
        // Pin boundaries that can change what source or model a grant refers to.
        let mut binding_value = json!({
            "identity": value["identity"], "policy": value["policy"], "sources": identities,
            "graph": value["graph"], "pdf": profile.pdf,
            "chat": endpoints.iter().map(|(name, endpoint)| (name, endpoint.fingerprint())).collect::<BTreeMap<_, _>>(),
            "embedding": embedding.as_ref().map(|e| (e.destination(), e.fingerprint())),
        });
        if let Some(worker) = &ocr {
            binding_value["ocr_fingerprint"] = json!(worker.fingerprint());
        }
        let binding = files::digest(&serde_json::to_vec(&binding_value)?);

        let lease = match new_private(&state.join("service.lock")) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                let path = state.join("service.lock");
                ensure!(bounded_file(&path, 0)?.is_empty(), "Invalid service lease.");
                OpenOptions::new().read(true).write(true).open(path)?
            }
            Err(error) => return Err(error),
        };
        lease
            .try_lock()
            .map_err(|_| anyhow::anyhow!("Another process owns this shared index."))?;
        let meta = Connection::open_with_flags(
            state.join("shared.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        meta.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        meta.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA secure_delete=ON; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS service_meta (id INTEGER PRIMARY KEY CHECK (id=1), binding TEXT NOT NULL,
                policy_version INTEGER NOT NULL, policy_digest TEXT NOT NULL, maintenance INTEGER NOT NULL,
                reconciled_policy TEXT, restore_pending INTEGER NOT NULL);")?;
        meta.execute(
            "INSERT OR IGNORE INTO service_meta VALUES (1,?1,0,'',0,NULL,0)",
            [&binding],
        )?;
        let saved: String =
            meta.query_row("SELECT binding FROM service_meta WHERE id=1", [], |row| {
                row.get(0)
            })?;
        ensure!(
            binding == saved,
            "This state is bound to a different shared deployment. Use a separate state directory."
        );
        let mut service = Self {
            auth,
            policy_source,
            meta: Mutex::new(meta),
            gate: RwLock::new(()),
            mutation: Mutex::new(()),
            job: Mutex::new(json!({"status":"idle"})),
            _lease: lease,
            state,
            binding,
            sources,
            _collection_connections: Mutex::new(Vec::new()),
            backups,
            pdf: profile.pdf,
            ocr,
            allow_proxy_http: profile.allow_proxy_http,
            endpoints,
            embedding,
            #[cfg(feature = "inference")]
            reranker,
            reranker_status,
            graph: profile.graph.mode,
            extractor: profile.graph.extractor,
            allow_unreviewed_model_links: profile.graph.allow_unreviewed_model_links,
            #[cfg(feature = "aws")]
            projection,
        };
        service.policy().map_err(|error| anyhow::anyhow!(error.2))?;
        service
            .auth
            .preflight()
            .context("The verification keys could not be loaded.")?;
        let mut connections = Vec::with_capacity(service.sources.len());
        for name in service.sources.keys() {
            let mut db = service.open_collection(name, true)?;
            store::upgrade(&mut db, &service.collection_dir(name))?;
            let journal: String = db.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
            ensure!(journal == "wal", "Shared collection storage requires WAL.");
            if let Some(embedding) = &service.embedding {
                let tx = db.transaction_with_behavior(Immediate)?;
                vector::configure(&tx, embedding.fingerprint(), embedding.dimensions())?;
                tx.commit()?;
            }
            connections.push(db);
        }
        service._collection_connections = Mutex::new(connections);
        Ok(service)
    }

    fn collection_dir(&self, name: &str) -> PathBuf {
        self.state.join("collections").join(name)
    }

    fn open_collection(&self, name: &str, create: bool) -> Result<Connection> {
        let path = files::state_directory(&self.collection_dir(name), create)?;
        let fingerprint = self.ocr.as_ref().map(crate::ocr::Worker::fingerprint);
        let db = store::open_shared(&path, create, fingerprint)?;
        if !create {
            let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
            ensure!(journal == "wal", "Shared collection storage requires WAL.");
        }
        ensure!(
            store::ocr_fingerprint(&db)?.as_deref() == fingerprint,
            "Unexpected shared OCR binding."
        );
        Ok(db)
    }

    fn policy(&self) -> ApiResult<(Policy, String)> {
        // Read and persist under a single lock so two request threads cannot
        // apply the policy floor out of order.
        let meta = self.meta.lock().map_err(|_| POLICY_ERROR)?;
        let (mut policy, digest) = self.policy_source.read().map_err(|_| POLICY_ERROR)?;
        let valid_set = |items: &[String], check: fn(&str) -> bool| {
            items.len() <= 5_000
                && items.iter().all(|s| check(s))
                && items.iter().collect::<BTreeSet<_>>().len() == items.len()
        };
        if policy.schema_version != 1
            || policy.version < 1
            || policy.issuer != self.auth.issuer
            || policy.administrators.is_empty()
            || !valid_set(&policy.administrators, subject)
            || policy.subjects.len() > 5_000
            || policy
                .subjects
                .iter()
                .any(|(sub, roles)| !subject(sub) || !valid_set(roles, label))
            || policy.collections.iter().any(|(name, grant)| {
                !self.sources.contains_key(name)
                    || !valid_set(&grant.subjects, subject)
                    || !valid_set(&grant.roles, label)
                    || !valid_set(&grant.endpoints, label)
                    || !valid_set(&grant.graph_review_subjects, subject)
                    || !valid_set(&grant.graph_review_roles, label)
                    || (!grant.reviewed_relationships.is_empty()
                        && (grant.reviewed_relationships.len() > 128
                            || grant.graph_publication.is_none()
                            || self.extractor.as_ref().is_none_or(|extractor| {
                                grant
                                    .reviewed_relationships
                                    .iter()
                                    .any(|edge| !edge.valid(self.endpoints[extractor].predicates()))
                            })))
                    || grant.graph_publication.as_ref().is_some_and(|rules| {
                        self.graph != GraphMode::Typed
                            || self.sources[name].folder().is_none()
                            || self.extractor.as_ref().is_none_or(|extractor| {
                                rules
                                    .validate(self.endpoints[extractor].predicates())
                                    .is_err()
                            })
                    })
                    || grant.endpoints.iter().any(|endpoint| {
                        !self.endpoints.contains_key(endpoint)
                            && self.embedding.as_ref().and_then(Embedding::destination)
                                != Some(endpoint)
                    })
            })
        {
            return Err(POLICY_ERROR);
        }
        let (version, last): (i64, String) = meta
            .query_row(
                "SELECT policy_version,policy_digest FROM service_meta WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| POLICY_ERROR)?;
        if policy.version < version || policy.version == version && digest != last {
            return Err(POLICY_ERROR);
        }
        if policy.version > version {
            meta.execute(
                "UPDATE service_meta SET policy_version=?1,policy_digest=?2 WHERE id=1",
                params![policy.version, digest],
            )
            .map_err(|_| POLICY_ERROR)?;
        }
        // The default has no trusted paths or automatic publication rules.
        // Keep the policy floor tied to the original signed/managed input.
        if self.graph == GraphMode::Typed
            && self.extractor.is_some()
            && !self.allow_unreviewed_model_links
        {
            for grant in policy.collections.values_mut() {
                grant
                    .graph_publication
                    .get_or_insert_with(PublicationPolicy::default);
            }
        }
        Ok((policy, digest))
    }

    fn publication_fingerprint(&self, rules: &PublicationPolicy) -> Result<String> {
        let extractor = self
            .extractor
            .as_ref()
            .context("Graph extractor is unavailable.")?;
        Ok(format!(
            "{REVIEW_FINGERPRINT_PREFIX}{}:{}",
            rules.digest()?,
            self.endpoints[extractor].fingerprint()
        ))
    }

    fn graph_publication_current(&self, db: &Connection, grant: &Grant) -> Result<bool> {
        let fingerprint: Option<String> = db
            .query_row(
                "SELECT fingerprint FROM graph_config WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(rules) = &grant.graph_publication else {
            return Ok(!fingerprint
                .is_some_and(|fingerprint| fingerprint.starts_with(REVIEW_FINGERPRINT_PREFIX)));
        };
        let expected = self.publication_fingerprint(rules)?;
        if fingerprint.as_deref() != Some(expected.as_str()) {
            return Ok(false);
        }
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM shared_declaration_state
             WHERE id=1 AND catalog_digest IS NOT NULL)",
            [],
            |row| row.get(0),
        )?)
    }

    fn reconcile_declarations(
        &self,
        db: &mut Connection,
        source: &Source,
        rules: &PublicationPolicy,
        fingerprint: &str,
        policy_version: i64,
    ) -> Result<()> {
        let extractor = self
            .extractor
            .as_ref()
            .context("Graph extractor is unavailable.")?;
        let predicates = self.endpoints[extractor].predicates();
        let root = source
            .folder()
            .context("Graph declarations require a folder source.")?;
        let catalog = Catalog::load(db, root, rules, predicates)?;
        let tx = db.transaction_with_behavior(Immediate)?;
        graph::configure(&tx, fingerprint)?;
        graph::install_shared_review(&tx)?;
        let stored = {
            let mut statement =
                tx.prepare("SELECT chunk_id,digest FROM shared_declaration_chunks")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        for chunk in stored
            .keys()
            .filter(|chunk| catalog.chunk_digest(chunk).is_none())
        {
            clear_declaration_chunk(&tx, chunk)?;
        }
        for chunk in catalog.chunk_ids() {
            let digest = catalog
                .chunk_digest(chunk)
                .context("Missing declaration chunk digest.")?;
            if stored.get(chunk).is_some_and(|previous| previous == digest) {
                continue;
            }
            clear_declaration_chunk(&tx, chunk)?;
            ensure!(
                graph::publish_shared(
                    &tx,
                    chunk,
                    fingerprint,
                    predicates,
                    &catalog.items(chunk),
                    policy_version,
                )?,
                "The declaration chunk changed before publication."
            );
            tx.execute(
                "INSERT INTO shared_declaration_chunks(chunk_id,fingerprint,digest) VALUES(?1,?2,?3)",
                params![chunk, fingerprint, digest],
            )?;
        }
        tx.execute(
            "INSERT INTO shared_declaration_state VALUES(1,?1)
             ON CONFLICT(id) DO UPDATE SET catalog_digest=excluded.catalog_digest",
            [catalog.digest()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn phase(&self) -> ApiResult<(bool, Option<String>, bool)> {
        let meta = self.meta.lock().map_err(unavailable)?;
        meta.query_row(
            "SELECT maintenance,reconciled_policy,restore_pending FROM service_meta WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(unavailable)
    }

    fn set_phase(
        &self,
        maintenance: bool,
        reconciled: Option<&str>,
        restore_pending: bool,
    ) -> ApiResult<()> {
        let meta = self.meta.lock().map_err(unavailable)?;
        meta.execute("UPDATE service_meta SET maintenance=?1,reconciled_policy=?2,restore_pending=?3 WHERE id=1",
            params![maintenance, reconciled, restore_pending]).map_err(unavailable)?;
        Ok(())
    }

    fn ready(&self) -> ApiResult<()> {
        if self.phase()?.0 {
            return Err(ApiError::new(
                503,
                "MAINTENANCE",
                "This index is closed until reconciliation and explicit reopen.",
            ));
        }
        Ok(())
    }

    fn read_during_sync(&self) -> ApiResult<RwLockReadGuard<'_, ()>> {
        let started = Instant::now();
        loop {
            match self.gate.try_read() {
                Ok(read) => return Ok(read),
                Err(TryLockError::Poisoned(error)) => return Err(unavailable(error)),
                Err(TryLockError::WouldBlock) => {
                    // Wait through short publication windows. Keep the wait bounded
                    // and do not queue readers behind backup, restore, or reopen.
                    if started.elapsed() >= Duration::from_millis(250) {
                        return Err(WRITER_BUSY);
                    }
                    if self.job.lock().map_err(unavailable)?["status"] != "running" {
                        // Sync may have released visibility while this reader
                        // was checking its status. Retry once before rejecting.
                        return match self.gate.try_read() {
                            Ok(read) => Ok(read),
                            Err(TryLockError::Poisoned(error)) => Err(unavailable(error)),
                            Err(TryLockError::WouldBlock) => Err(WRITER_BUSY),
                        };
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    fn check_scope(
        &self,
        principal: &Principal,
        scope: &[String],
        endpoint: Option<(&str, &[String])>,
        graph_policy: Option<&Policy>,
    ) -> ApiResult<()> {
        let (policy, _) = self.policy()?;
        if policy.scope(principal)? != scope {
            return Err(ACCESS_CHANGED);
        }
        if let Some((name, collections)) = endpoint {
            for collection in collections {
                policy.destination(collection, name)?;
            }
        }
        if graph_policy.is_some_and(|expected| {
            scope.iter().any(|name| {
                expected.collections[name].graph_publication
                    != policy.collections[name].graph_publication
            })
        }) {
            return Err(ACCESS_CHANGED);
        }
        self.ready()
    }

    fn confirm_admin(&self, principal: &Principal, digest: &str) -> ApiResult<Policy> {
        let (policy, current) = self.policy()?;
        policy.admin(principal)?;
        if current != digest {
            return Err(ACCESS_CHANGED);
        }
        Ok(policy)
    }

    fn query(&self, principal: &Principal, request: Query) -> ApiResult<Value> {
        self.query_with_reranker(principal, request, |question, evidence| {
            #[cfg(feature = "inference")]
            if let Some(reranker) = &self.reranker {
                return reranker.reorder(question, evidence);
            }
            #[cfg(not(feature = "inference"))]
            let _ = (question, evidence);
            json!({"status":"fallback","reason":"unavailable"})
        })
    }

    fn query_with_reranker(
        &self,
        principal: &Principal,
        request: Query,
        rerank: impl FnOnce(&str, &mut [Value]) -> Value,
    ) -> ApiResult<Value> {
        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, _) = self.policy()?;
        let scope = policy.scope(principal)?;
        let limit = request.limit.unwrap_or(5);
        if !(1..=20).contains(&limit) || store::validate_query(&request.question, limit).is_err() {
            return Err(ApiError::new(
                400,
                "QUERY_LIMIT",
                "Use a question of up to 4,000 characters and a result limit of 1–20.",
            ));
        }
        let mode = match request.mode.as_deref().unwrap_or("lexical") {
            "lexical" => Mode::Lexical,
            "vector" => Mode::Vector,
            "hybrid" => Mode::Hybrid,
            _ => {
                return Err(ApiError::new(
                    400,
                    "QUERY_MODE",
                    "Choose lexical, vector or hybrid retrieval.",
                ));
            }
        };
        let graph = request.graph.unwrap_or(self.graph != GraphMode::Off);
        if graph && self.graph == GraphMode::Off {
            return Err(ApiError::new(
                400,
                "GRAPH_UNAVAILABLE",
                "Choose a graph-enabled deployment.",
            ));
        }
        let graph_policy = (graph && self.graph == GraphMode::Typed).then_some(&policy);
        let answer = request
            .answer_endpoint
            .as_ref()
            .map(|name| {
                self.endpoints
                    .get(name)
                    .map(|ep| (name, ep))
                    .ok_or(ApiError::new(
                        400,
                        "MODEL_ENDPOINT",
                        "Select a configured answer endpoint by name.",
                    ))
            })
            .transpose()?;
        let embedding = if mode != Mode::Lexical {
            Some(self.embedding.as_ref().ok_or(ApiError::new(
                400,
                "EMBEDDING_UNAVAILABLE",
                "Configure an embedding provider for vector or hybrid retrieval.",
            ))?)
        } else {
            None
        };
        let mut candidates = Vec::new();
        let candidate_depth = limit;
        #[cfg(feature = "inference")]
        let candidate_depth = if request.rerank && self.reranker.is_some() {
            crate::inference::RERANK_DEPTH
        } else {
            candidate_depth
        };
        let mut effective_mode = mode;
        let mut embedding_failure = None;
        let mut vector_indexed = 0;
        let mut vector_pending = 0;
        let mut graph_coverage = BTreeMap::new();
        #[cfg(feature = "aws")]
        let projection_scope = (graph && self.projection.is_some()).then(|| {
            scope
                .iter()
                .map(|name| {
                    let mut db = self.open_collection(name, false)?;
                    Ok((name.clone(), crate::cloud::projection_scope(&mut db)?))
                })
                .collect::<Result<BTreeMap<_, _>>>()
        });
        // A caller without readable collections must never trigger egress,
        // even if it selects vector mode or requests an answer endpoint.
        let embedded = if let Some(embedding) = embedding.filter(|_| !scope.is_empty()) {
            let dest = embedding.destination().map(|name| (name, scope.as_slice()));
            self.check_scope(principal, &scope, dest, graph_policy)?;
            let values = embedding.embed(&request.question, true);
            self.check_scope(principal, &scope, dest, graph_policy)?;
            match values {
                Ok(values) => Some((embedding, values)),
                Err(error) if mode == Mode::Hybrid => {
                    embedding_failure = Some(Failure::of(&error).code());
                    effective_mode = Mode::Lexical;
                    None
                }
                Err(error) => return Err(model_error(Failure::of(&error))),
            }
        } else {
            None
        };
        for (collection_order, collection) in scope.iter().enumerate() {
            let mut db = self
                .open_collection(collection, false)
                .map_err(unavailable)?;
            let traverse = graph
                && self
                    .graph_publication_current(&db, &policy.collections[collection])
                    .map_err(unavailable)?;
            let vector = embedded.as_ref().map(|(embedding, values)| VectorQuery {
                fingerprint: embedding.fingerprint(),
                values,
            });
            #[cfg(feature = "aws")]
            let result =
                if let (Some(projection), Some(scopes)) = (&self.projection, &projection_scope) {
                    let lookup = |entity: &str, limit| {
                        self.check_scope(principal, &scope, None, graph_policy)
                            .map_err(|_| anyhow::anyhow!("Access changed."))?;
                        let scopes = scopes.as_ref().map_err(|_| {
                            anyhow::anyhow!("Projection scope could not be verified.")
                        })?;
                        projection.scoped_neighbors(entity, collection, scopes, limit)
                    };
                    store::query_with_neighbors(
                        &mut db,
                        &request.question,
                        candidate_depth,
                        effective_mode,
                        vector,
                        &lookup,
                    )
                } else {
                    store::query(
                        &mut db,
                        &request.question,
                        candidate_depth,
                        traverse,
                        effective_mode,
                        vector,
                    )
                }
                .map_err(unavailable)?;
            #[cfg(not(feature = "aws"))]
            let result = store::query(
                &mut db,
                &request.question,
                candidate_depth,
                traverse,
                effective_mode,
                vector,
            )
            .map_err(unavailable)?;
            if graph {
                let mut coverage = if traverse {
                    result["graph_coverage"].clone()
                } else {
                    json!({"status":"publication_reconciling"})
                };
                if traverse && self.graph == GraphMode::Typed {
                    let rejected = graph::shared_rejections(&db).map_err(unavailable)?;
                    if rejected["chunks"].as_i64().is_some_and(|count| count > 0) {
                        coverage["partial"] = json!(true);
                        coverage["rejected_anchor_chunks"] = rejected["chunks"].clone();
                    }
                }
                graph_coverage.insert(collection, coverage);
            }
            vector_indexed += result["vector_coverage"]["indexed_chunks"]
                .as_u64()
                .unwrap_or_default();
            vector_pending += result["vector_coverage"]["pending_chunks"]
                .as_u64()
                .unwrap_or_default();
            let evidence = result["evidence"]
                .as_array()
                .cloned()
                .ok_or_else(|| unavailable("missing evidence"))?;
            for (rank, mut item) in evidence.into_iter().enumerate() {
                self.sources[collection]
                    .cite(collection, &mut item)
                    .map_err(unavailable)?;
                let weak = item["match_status"] != "evidence";
                candidates.push(((weak, rank, collection_order), item));
            }
        }
        // Prefer strong matches, then compare collection-local ranks. Raw
        // scores never enter the merge because each collection has its own index.
        candidates.sort_unstable_by_key(|(key, _)| *key);
        let mut evidence: Vec<_> = candidates
            .into_iter()
            .take(candidate_depth)
            .map(|(_, item)| item)
            .collect();
        let mut reranking = None;
        if request.rerank {
            self.check_scope(principal, &scope, None, graph_policy)?;
            reranking = Some(rerank(&request.question, &mut evidence));
            self.check_scope(principal, &scope, None, graph_policy)?;
        }
        evidence.truncate(limit);
        let mut graph_paths = BTreeMap::new();
        if graph && self.graph == GraphMode::Typed {
            for collection in &scope {
                if graph_coverage[collection]["status"] == "publication_reconciling" {
                    continue;
                }
                let delivered: Vec<_> = evidence
                    .iter()
                    .filter(|row| row["collection"] == collection.as_str())
                    .collect();
                if !delivered.is_empty() {
                    let db = self
                        .open_collection(collection, false)
                        .map_err(unavailable)?;
                    let paths = graph::delivered_paths(&db, &request.question, &delivered)
                        .map_err(unavailable)?;
                    graph_paths.insert(collection.clone(), paths);
                }
            }
        }
        let status = if evidence.is_empty() {
            "abstained"
        } else if evidence.iter().any(|hit| hit["match_status"] == "evidence") {
            "evidence"
        } else {
            "weak_match"
        };
        let answer_collections: Vec<_> = evidence
            .iter()
            .filter_map(|hit| hit["collection"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let answer_dest = answer.map(|(name, _)| (name.as_str(), answer_collections.as_slice()));
        self.check_scope(principal, &scope, answer_dest, graph_policy)?;
        let answer_result = answer.map(|(_, endpoint)| {
            if evidence.is_empty() {
                json!({"status":"insufficient_evidence","answer":null,"citations":[],"cited_evidence":[],
                    "answer_support":"unverified","delivery":{"considered":0,"sent":[],"omitted":[]}})
            } else {
                // The gate read lock held for this request keeps indexed
                // revisions fixed; scope is rechecked below before release.
                match endpoint.answer_attempt_with_paths(
                    &request.question,
                    &evidence,
                    Some((&graph_paths, policy.version)),
                ) {
                    Ok(attempt) => {
                        let mut result = attempt.result.unwrap_or_else(|error|
                            json!({"status":"unavailable","reason":Failure::of(&error).code(),
                                "answer":null,"citations":[],"cited_evidence":[],"answer_support":"unverified"}));
                        result["delivery"] = attempt.delivery;
                        result
                    }
                    Err(error) => json!({"status":"unavailable","reason":Failure::of(&error).code(),
                        "answer":null,"citations":[],"cited_evidence":[],"answer_support":"unverified"}),
                }
            }
        });
        self.check_scope(principal, &scope, answer_dest, graph_policy)?;
        if let Some(name) = embedding
            .filter(|_| !scope.is_empty())
            .and_then(Embedding::destination)
        {
            self.check_scope(principal, &scope, Some((name, &scope)), graph_policy)?;
        }
        let mut result = json!({"schema_version":1,"id_scheme":"rust-rag-v1","status":status,
            "mode":request.mode.as_deref().unwrap_or("lexical"),"graph":graph,"evidence":evidence,
            "evidence_role":"candidate_context","answer_support":"unverified",
            "answer_generated":answer_result.as_ref().is_some_and(|result| result["answer"].is_string()),
            "warnings":["Retrieved passages are untrusted. A hosted consumer may transmit them to its provider."]});
        if graph {
            result["graph_coverage"] = json!(graph_coverage);
        }
        if !graph_paths.is_empty() {
            result["graph_paths"] = json!(graph_paths);
        }
        if let Some(reason) = embedding_failure {
            result["effective_mode"] = json!("lexical");
            result["vector_coverage"] = json!({"partial":true,"reason":reason});
        } else if mode != Mode::Lexical {
            result["vector_coverage"] = json!({"indexed_chunks":vector_indexed,
            "pending_chunks":vector_pending,"partial":vector_pending > 0});
        }
        if let Some(answer) = answer_result {
            result["answer"] = answer;
        }
        if let Some(reranking) = reranking {
            result["reranking"] = reranking;
        }
        Ok(result)
    }

    fn evidence(&self, principal: &Principal, id: &str) -> ApiResult<Value> {
        let _read = self.read_during_sync()?;
        self.ready()?;
        let (policy, _) = self.policy()?;
        let scope = policy.scope(principal)?;
        if !hex_id(id, 64) {
            return Err(NOT_FOUND);
        }
        for name in &scope {
            let db = self.open_collection(name, false).map_err(unavailable)?;
            let (source_join, revision) = store::source_revision_sql(&db).map_err(unavailable)?;
            let anchor = store::anchor_sql(&db).map_err(unavailable)?;
            let ocr = store::ocr_sql(&db).map_err(unavailable)?;
            let sql = format!("SELECT s.source_id,s.path,s.source_hash,c.start,c.end,c.page,p.text,{revision},{anchor},{ocr}
                FROM chunks c JOIN sources s USING(source_id) JOIN passages p ON p.rowid=c.row_id {source_join} WHERE c.chunk_id=?1");
            let evidence = db.query_row(&sql, [id], |row| {
                let hash: String = row.get(2)?;
                let revision: String = row.get(7)?;
                let mut item = json!({"chunk_id":id,"source_id":row.get::<_,String>(0)?,"path":row.get::<_,String>(1)?,
                    "source_hash":hash,"revision":revision,"generation":hash,"start":row.get::<_,i64>(3)?,
                    "end":row.get::<_,i64>(4)?,"page":row.get::<_,Option<i64>>(5)?,"text":row.get::<_,String>(6)?,
                    "anchor":row.get::<_,Option<String>>(8)?,
                    "evidence_role":"candidate_context","answer_support":"unverified"});
                store::attach_ocr(&mut item, row, 9)?;
                Ok(item)
            }).optional().map_err(unavailable)?;
            if let Some(mut item) = evidence {
                self.sources[name]
                    .cite(name, &mut item)
                    .map_err(unavailable)?;
                self.check_scope(principal, &scope, None, None)?;
                return Ok(json!({"schema_version":1,"evidence":item}));
            }
        }
        self.check_scope(principal, &scope, None, None)?;
        Err(NOT_FOUND)
    }

    fn admin_status(&self, principal: &Principal) -> ApiResult<Value> {
        self.policy()?.0.admin(principal)?;
        let (maintenance, _, restore_pending) = self.phase()?;
        let job = self.job.lock().map_err(unavailable)?.clone();
        Ok(
            json!({"schema_version":1,"maintenance":maintenance,"restore_pending":restore_pending,"sync":job}),
        )
    }

    fn graph_review_list(
        &self,
        principal: &Principal,
        request: GraphReviewList,
    ) -> ApiResult<Value> {
        let _read = self.gate.try_read().map_err(|_| WRITER_BUSY)?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let grant = policy.graph_reviewer(principal, &request.collection)?;
        let rules = grant.graph_publication.as_ref().ok_or(REVIEW_STALE)?;
        let db = self
            .open_collection(&request.collection, false)
            .map_err(unavailable)?;
        if !self
            .graph_publication_current(&db, grant)
            .map_err(unavailable)?
        {
            return Err(REVIEW_STALE);
        }
        let fingerprint = self.publication_fingerprint(rules).map_err(unavailable)?;
        let mut result = graph::shared_review_queue(
            &db,
            &fingerprint,
            request.after.as_deref(),
            request.limit.unwrap_or(20),
        )
        .map_err(graph_review_error)?;
        let rows = result["proposals"]
            .as_array_mut()
            .ok_or_else(|| unavailable("missing proposals"))?;
        for row in rows {
            self.sources[&request.collection]
                .cite(&request.collection, row)
                .map_err(unavailable)?;
            row["staged_policy_version"] = row["policy_version"].clone();
            row["policy_version"] = json!(policy.version);
        }
        result["collection"] = json!(request.collection);
        result["policy_version"] = json!(policy.version);
        self.confirm_graph_reviewer(principal, &request.collection, &digest)?;
        Ok(result)
    }

    fn confirm_graph_reviewer(
        &self,
        principal: &Principal,
        collection: &str,
        digest: &str,
    ) -> ApiResult<()> {
        let (policy, current) = self.policy()?;
        policy.graph_reviewer(principal, collection)?;
        if current != digest {
            return Err(ACCESS_CHANGED);
        }
        self.ready()
    }

    fn graph_review_decide(
        &self,
        principal: &Principal,
        request: GraphReviewRequest,
    ) -> ApiResult<Value> {
        let _write = self.try_write()?;
        self.ready()?;
        let (policy, digest) = self.policy()?;
        let grant = policy.graph_reviewer(principal, &request.collection)?;
        let rules = grant.graph_publication.as_ref().ok_or(REVIEW_STALE)?;
        let fingerprint = self.publication_fingerprint(rules).map_err(unavailable)?;
        if request.policy_version != policy.version || request.fingerprint != fingerprint {
            return Err(REVIEW_STALE);
        }
        let mut db = self
            .open_collection(&request.collection, false)
            .map_err(unavailable)?;
        if !self
            .graph_publication_current(&db, grant)
            .map_err(unavailable)?
        {
            return Err(REVIEW_STALE);
        }
        let root = self.sources[&request.collection]
            .folder()
            .ok_or(REVIEW_STALE)?;
        let tx = db
            .transaction_with_behavior(Immediate)
            .map_err(unavailable)?;
        let result = graph::decide_shared(
            &tx,
            root,
            &graph::SharedDecision {
                id: &request.proposal_id,
                approve: matches!(request.decision, ReviewDecision::Approve),
                revision: &request.revision,
                fingerprint: &fingerprint,
                policy_version: policy.version,
                actor: &principal.subject,
                idempotency_key: &request.idempotency_key,
            },
        )
        .map_err(graph_review_error)?;
        self.confirm_graph_reviewer(principal, &request.collection, &digest)?;
        tx.commit().map_err(unavailable)?;
        Ok(result)
    }

    fn try_write(&self) -> ApiResult<ServiceWriter<'_>> {
        let reservation = self.mutation.try_lock().map_err(|_| WRITER_BUSY)?;
        let visibility = self.gate.try_write().map_err(|_| WRITER_BUSY)?;
        Ok(ServiceWriter {
            visibility: Some(visibility),
            gate: &self.gate,
            _reservation: reservation,
        })
    }

    fn start_sync(self: &Arc<Self>, principal: Principal) -> ApiResult<Value> {
        let (admitted, reply) = mpsc::sync_channel(1);
        let service = Arc::clone(self);
        thread::Builder::new()
            .name("rust-rag-sync".into())
            .spawn(move || {
                // Hold the gate in the worker that will use it. A busy reply
                // leaves the policy floor, job state and sources untouched.
                let admission = (|| {
                    let mut job = service.job.lock().map_err(unavailable)?;
                    let write = service.try_write()?;
                    let (policy, digest) = service.policy()?;
                    policy.admin(&principal)?;
                    *job = json!({"status":"running"});
                    Ok((write, digest))
                })();
                let (mut write, digest) = match admission {
                    Ok(admission) => admission,
                    Err(error) => {
                        let _ = admitted.send(Err(error));
                        return;
                    }
                };
                let _ = admitted.send(Ok(()));
                let report = service
                    .sync(&principal, &digest, &mut write)
                    .unwrap_or_else(|error| json!({"status":"incomplete","reason":error.1}));
                if let Ok(mut job) = service.job.lock() {
                    // Keep the reservation until the terminal state is visible.
                    drop(write.visibility.take());
                    *job = report;
                }
            })
            .map_err(unavailable)?;
        reply.recv().map_err(unavailable)??;
        Ok(json!({"schema_version":1,"status":"running"}))
    }

    fn sync(
        &self,
        principal: &Principal,
        digest: &str,
        write: &mut ServiceWriter<'_>,
    ) -> ApiResult<Value> {
        // Sync reserves the writer until it finishes. Slow external work can
        // let readers through without giving up that reservation.
        let policy = self.confirm_admin(principal, digest)?;
        let (maintenance, _, restore_pending) = self.phase()?;
        if restore_pending {
            return Err(ApiError::new(
                409,
                "RESTORE_INCOMPLETE",
                "Retry the backup restore before reconciliation.",
            ));
        }
        if maintenance {
            self.set_phase(true, None, false)?;
        }
        let mut updated = 0;
        let mut unchanged = 0;
        let mut removed = 0;
        let mut failed = 0;
        let mut graph_pending = 0;
        let mut graph_rejected = 0;
        let mut vector_pending = 0;
        let mut enrichment = BTreeMap::new();
        let mut pauses = [None, None];
        let mut sync_blocked = false;
        let mut source_pending = 0;
        let mut projection_complete = true;
        let mut projection_cleanup_complete = true;
        let mut wal_checkpoint = BTreeMap::new();
        let check_policy = || {
            self.confirm_admin(principal, digest)
                .map_err(|error| anyhow::anyhow!(error.2))
        };
        for (name, source) in &self.sources {
            self.confirm_admin(principal, digest)?;
            let mut db = self.open_collection(name, false).map_err(unavailable)?;
            let Some(grant) = policy.collections.get(name) else {
                removed += store::remove(&mut db, None).map_err(unavailable)? as u64;
                continue;
            };
            let review_fingerprint = grant
                .graph_publication
                .as_ref()
                .map(|rules| self.publication_fingerprint(rules))
                .transpose()
                .map_err(unavailable)?;
            if review_fingerprint.is_some() {
                let tx = db
                    .transaction_with_behavior(Immediate)
                    .map_err(unavailable)?;
                tx.execute_batch(DECLARATION_STATE).map_err(unavailable)?;
                tx.execute(
                    "INSERT INTO shared_declaration_state VALUES(1,NULL)
                     ON CONFLICT(id) DO UPDATE SET catalog_digest=NULL",
                    [],
                )
                .map_err(unavailable)?;
                tx.commit().map_err(unavailable)?;
            }
            for (work, paused) in self.permitted_lanes(grant).into_iter().zip(&mut pauses) {
                if let Some(work) = work {
                    let (lane, fingerprint) = work.key_with_review(review_fingerprint.as_deref());
                    *paused = (*paused).max(
                        enrichment::paused_until(&db, lane, fingerprint).map_err(unavailable)?,
                    );
                }
            }
            let mut extract = |worker: &crate::ocr::Worker, raw: &[u8], hash: &str, deadline| {
                check_policy()?;
                let result = worker.extract(raw, hash, deadline);
                check_policy()?;
                Ok(result)
            };
            let mut ocr = store::OcrBatch::new(self.ocr.as_ref()).with_extraction(&mut extract);
            let report = if source.folder().is_some() {
                let mut ocr = ocr.with_publish_gate(&self.gate);
                write.allow_reads(|| source.ingest(&mut db, self.pdf, &mut ocr))?
            } else {
                source.ingest(&mut db, self.pdf, &mut ocr)
            };
            let report = match report {
                Ok(report) => report,
                Err(error) => {
                    self.confirm_admin(principal, digest)?;
                    return Err(unavailable(error));
                }
            };
            updated += report["updated_sources"].as_u64().unwrap_or_default();
            unchanged += report["unchanged_sources"].as_u64().unwrap_or_default();
            removed += report["removed_sources"].as_u64().unwrap_or_default();
            failed += report["failures"].as_array().map_or(0, Vec::len);
            sync_blocked |= report["sync_blocked"].as_bool().unwrap_or(true);
            source_pending += report["priority_deferred_sources"]
                .as_u64()
                .unwrap_or_default();
        }
        let now = auth::epoch().map_err(unavailable)? as i64;
        let [mut graph_failure, mut vector_failure] = pauses.map(|pause| {
            pause
                .filter(|at| *at > now)
                .map(|at| Failure::RateLimited((at - now) as u64))
        });
        for name in self.sources.keys() {
            self.confirm_admin(principal, digest)?;
            let mut db = self.open_collection(name, false).map_err(unavailable)?;
            // Do not send restored or stale content out while any inventory is
            // incomplete. Existing assertions and vectors still retire with sources.
            if !sync_blocked
                && source_pending == 0
                && failed == 0
                && let Some(grant) = policy.collections.get(name)
            {
                let mut progress = serde_json::Map::new();
                let [graph_work, vector_work] = self.permitted_lanes(grant);
                let review_fingerprint = grant
                    .graph_publication
                    .as_ref()
                    .map(|rules| self.publication_fingerprint(rules))
                    .transpose()
                    .map_err(unavailable)?;
                let review = grant
                    .graph_publication
                    .as_ref()
                    .zip(review_fingerprint.as_deref());
                if let Some((rules, fingerprint)) = review {
                    self.reconcile_declarations(
                        &mut db,
                        &self.sources[name],
                        rules,
                        fingerprint,
                        policy.version,
                    )
                    .map_err(unavailable)?;
                    self.confirm_admin(principal, digest)?;
                }
                if let Some(work) = graph_work {
                    let mut lane = self.sync_enrichment(
                        &mut db,
                        principal,
                        digest,
                        name,
                        work,
                        review.map(|(rules, fingerprint)| (rules, fingerprint, policy.version)),
                        &mut graph_failure,
                        write,
                    )?;
                    graph_pending += lane["pending"].as_u64().unwrap_or_default();
                    if review.is_some() {
                        let rejected = graph::shared_rejections(&db).map_err(unavailable)?;
                        graph_rejected += rejected["chunks"].as_u64().unwrap_or_default();
                        lane["anchor_rejections"] = rejected;
                    }
                    progress.insert("graph".into(), lane);
                }
                if let Some(work) = vector_work {
                    let lane = self.sync_enrichment(
                        &mut db,
                        principal,
                        digest,
                        name,
                        work,
                        None,
                        &mut vector_failure,
                        write,
                    )?;
                    vector_pending += lane["pending"].as_u64().unwrap_or_default();
                    progress.insert("vector".into(), lane);
                }
                if !progress.is_empty() {
                    enrichment.insert(name, progress);
                }
            }
            self.confirm_admin(principal, digest)?;
            let (active, cleaned) = self.reconcile_graph(&mut db, name);
            projection_complete &= active;
            projection_cleanup_complete &= cleaned;
            wal_checkpoint.insert(name, write.allow_reads(|| Self::checkpoint(&db))?);
        }
        self.confirm_admin(principal, digest)?;
        let reconciled = !sync_blocked && source_pending == 0 && failed == 0 && projection_complete;
        if maintenance && reconciled {
            self.set_phase(true, Some(digest), false)?;
        }
        let status =
            if reconciled && graph_pending == 0 && graph_rejected == 0 && vector_pending == 0 {
                "complete"
            } else {
                "partial"
            };
        Ok(
            json!({"status":status,"updated":updated,"unchanged":unchanged,"removed":removed,"failed":failed,
            "source_pending":source_pending,
            "graph_pending":graph_pending,"graph_rejected":graph_rejected,
            "vector_pending":vector_pending,"sync_blocked":sync_blocked,
            "enrichment":enrichment,
            "provider_halts":{"graph":graph_failure.map(Failure::code),"vector":vector_failure.map(Failure::code)},
            "projection_complete":projection_complete,"projection_cleanup_complete":projection_cleanup_complete,
            "wal_checkpoint":wal_checkpoint,
            "inventory_reconciled":reconciled}),
        )
    }

    fn checkpoint(db: &Connection) -> &'static str {
        // Do not wait for an older reader to release its snapshot. A busy
        // checkpoint does not undo committed source or citation retirement.
        if db.busy_timeout(Duration::ZERO).is_err() {
            return "unavailable";
        }
        match db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        }) {
            Ok((0, 0, 0)) => "complete",
            Ok((1, _, _)) => "busy",
            _ => "unavailable",
        }
    }

    fn sync_enrichment(
        &self,
        db: &mut Connection,
        principal: &Principal,
        digest: &str,
        collection: &str,
        work: ConfiguredLane<'_>,
        review: Option<(&PublicationPolicy, &str, i64)>,
        halted: &mut Option<Failure>,
        write: &mut ServiceWriter<'_>,
    ) -> ApiResult<Value> {
        let (lane, fingerprint) =
            work.key_with_review(review.map(|(_, fingerprint, _)| fingerprint));
        let destination = work.destination();
        let tx = db
            .transaction_with_behavior(Immediate)
            .map_err(unavailable)?;
        match work {
            ConfiguredLane::Graph(..) => graph::configure(&tx, fingerprint),
            ConfiguredLane::Vector(embedding) => {
                vector::configure(&tx, fingerprint, embedding.dimensions())
            }
        }
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;

        let batches = if matches!(work, ConfiguredLane::Vector(Embedding::Hash { .. })) {
            64
        } else {
            1
        };
        let check_access = || -> ApiResult<()> {
            let policy = self.confirm_admin(principal, digest)?;
            if let Some(name) = destination {
                policy.destination(collection, name)?;
            }
            Ok(())
        };
        'batches: for _ in 0..batches {
            if halted.is_some() {
                break;
            }
            check_access()?;
            let pending = enrichment::ready(db, lane, fingerprint, 256).map_err(unavailable)?;
            if pending.is_empty() {
                break;
            }
            for chunk in pending {
                // A lease changes scheduling metadata only. The writer
                // reservation prevents competing mutations until publication.
                let prepared = write.allow_reads(|| -> ApiResult<_> {
                    let tx = db
                        .transaction_with_behavior(Immediate)
                        .map_err(unavailable)?;
                    let Some(claim) =
                        enrichment::claim(&tx, lane, fingerprint, &chunk).map_err(unavailable)?
                    else {
                        return Ok(None);
                    };
                    tx.commit().map_err(unavailable)?;
                    let passage = match lane {
                        Lane::Graph => graph::unfinished_text(db, &chunk, fingerprint),
                        Lane::Vector => vector::unfinished_text(db, &chunk, fingerprint),
                    }
                    .map_err(unavailable)?;
                    let Some(passage) = passage else {
                        return Ok(None);
                    };
                    check_access()?;
                    let payload = match work {
                        ConfiguredLane::Graph(_, endpoint) => {
                            endpoint.extract(&passage).map(PreparedEnrichment::Graph)
                        }
                        ConfiguredLane::Vector(embedding) => embedding
                            .embed(&passage, false)
                            .map(PreparedEnrichment::Vector),
                    };
                    Ok(Some((claim, payload)))
                })??;
                let Some((claim, payload)) = prepared else {
                    continue;
                };
                write.allow_reads(|| -> ApiResult<()> {
                    check_access()?;
                    let tx = db
                        .transaction_with_behavior(Immediate)
                        .map_err(unavailable)?;
                    if !claim.current(&tx).map_err(unavailable)? {
                        return Ok(());
                    }
                    let publication = match payload {
                        Ok(PreparedEnrichment::Graph(assertions)) => {
                            let ConfiguredLane::Graph(_, endpoint) = work else {
                                unreachable!("graph payload is from the configured graph lane");
                            };
                            let published = if let Some((rules, _, policy_version)) = review {
                                let items = rules.stage_proposals(assertions);
                                graph::publish_shared(
                                    &tx,
                                    &chunk,
                                    fingerprint,
                                    endpoint.predicates(),
                                    &items,
                                    policy_version,
                                )
                            } else {
                                graph::publish(
                                    &tx,
                                    &chunk,
                                    fingerprint,
                                    endpoint.predicates(),
                                    &assertions,
                                )
                            };
                            match published {
                                Ok(published) => Ok(published),
                                // Graph validation finishes before publication writes.
                                Err(error) if error.downcast_ref::<rusqlite::Error>().is_none() => {
                                    Err(Failure::Item)
                                }
                                Err(error) => return Err(unavailable(error)),
                            }
                        }
                        Ok(PreparedEnrichment::Vector(values)) => {
                            Ok(vector::publish(&tx, &chunk, fingerprint, &values)
                                .map_err(unavailable)?)
                        }
                        Err(error) => Err(Failure::of(&error)),
                    };
                    let current = match publication {
                        Ok(true) => enrichment::completed(&tx, &claim),
                        Ok(false) => enrichment::release(&tx, &claim),
                        Err(Failure::Item) => enrichment::failed(&tx, &claim),
                        Err(failure) => {
                            *halted = Some(failure);
                            if let Failure::RateLimited(seconds) = failure {
                                enrichment::pause(&tx, lane, fingerprint, seconds)
                                    .map_err(unavailable)?;
                            }
                            enrichment::release(&tx, &claim)
                        }
                    }
                    .map_err(unavailable)?;
                    if current {
                        check_access()?;
                        tx.commit().map_err(unavailable)?;
                    }
                    Ok(())
                })??;
                if halted.is_some() {
                    break 'batches;
                }
            }
        }
        enrichment::progress(db, lane, fingerprint).map_err(unavailable)
    }

    fn retry_enrichment(
        &self,
        principal: &Principal,
        request: RetryEnrichment,
    ) -> ApiResult<Value> {
        let _write = self.try_write()?;
        let (policy, digest) = self.policy()?;
        policy.admin(principal)?;
        let invalid = ApiError::new(
            400,
            "ENRICHMENT_LANE",
            "Choose a configured collection and its graph or vector lane.",
        );
        if !policy.collections.contains_key(&request.collection) {
            return Err(invalid);
        }
        let work = match request.lane.as_str() {
            "graph" => self
                .extractor
                .as_ref()
                .map(|name| ConfiguredLane::Graph(name, &self.endpoints[name])),
            "vector" => self.embedding.as_ref().map(ConfiguredLane::Vector),
            _ => None,
        }
        .ok_or(invalid)?;
        let review_fingerprint = policy.collections[&request.collection]
            .graph_publication
            .as_ref()
            .map(|rules| self.publication_fingerprint(rules))
            .transpose()
            .map_err(unavailable)?;
        let (lane, fingerprint) = work.key_with_review(review_fingerprint.as_deref());
        let current = self.confirm_admin(principal, &digest)?;
        if let Some(name) = work.destination() {
            current.destination(&request.collection, name)?;
        }
        let mut db = self
            .open_collection(&request.collection, false)
            .map_err(unavailable)?;
        let tx = db
            .transaction_with_behavior(Immediate)
            .map_err(unavailable)?;
        let mut reset = enrichment::retry(&tx, lane, fingerprint).map_err(unavailable)?;
        let reviewed_graph = request.lane == "graph" && review_fingerprint.is_some();
        if reviewed_graph {
            reset += graph::retry_rejected_chunks(&tx, fingerprint).map_err(unavailable)?;
        }
        let mut progress = enrichment::progress(&tx, lane, fingerprint).map_err(unavailable)?;
        if reviewed_graph {
            progress["anchor_rejections"] = graph::shared_rejections(&tx).map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(json!({"schema_version":1,"reset":reset,"progress":progress}))
    }

    fn reconcile_graph(&self, _db: &mut Connection, _name: &str) -> (bool, bool) {
        #[cfg(feature = "aws")]
        if let Some(projection) = &self.projection {
            // An active generation excludes old edges even if their physical
            // deletion fails. Keep that retention work visible to operators.
            return match projection.reconcile(_db, _name) {
                Ok(cleaned) => (true, cleaned),
                Err(_) => (false, false),
            };
        }
        (true, true)
    }

    fn remove(&self, principal: &Principal, source: &str) -> ApiResult<Value> {
        if !hex_id(source, 64) {
            return Err(ApiError::new(
                400,
                "SOURCE_ID",
                "Supply a source identifier from a citation.",
            ));
        }
        let mut write = self.try_write()?;
        let (policy, digest) = self.policy()?;
        policy.admin(principal)?;
        let (maintenance, _, pending) = self.phase()?;
        if pending {
            return Err(ApiError::new(
                409,
                "RESTORE_INCOMPLETE",
                "Retry the backup restore first.",
            ));
        }
        if maintenance {
            self.set_phase(true, None, false)?;
        }
        let mut removed = 0;
        let mut projected = true;
        let mut projection_cleanup_complete = true;
        let mut wal_checkpoint = BTreeMap::new();
        for name in self.sources.keys() {
            self.confirm_admin(principal, &digest)?;
            let mut db = self.open_collection(name, false).map_err(unavailable)?;
            let count = store::remove(&mut db, Some(source)).map_err(unavailable)?;
            if count > 0 {
                let (active, cleaned) = self.reconcile_graph(&mut db, name);
                projected &= active;
                projection_cleanup_complete &= cleaned;
            }
            removed += count;
            wal_checkpoint.insert(name, write.allow_reads(|| Self::checkpoint(&db))?);
        }
        self.confirm_admin(principal, &digest)?;
        Ok(
            json!({"schema_version":1,"removed_sources":removed,"projection_complete":projected,
            "projection_cleanup_complete":projection_cleanup_complete,
            "wal_checkpoint":wal_checkpoint,
            "scope":"Active indexes only. Source files and backup retention are separate."}),
        )
    }

    fn reopen(&self, principal: &Principal) -> ApiResult<Value> {
        let _write = self.try_write()?;
        let (policy, digest) = self.policy()?;
        policy.admin(principal)?;
        let (maintenance, reconciled, pending) = self.phase()?;
        if !maintenance || pending || reconciled.as_deref() != Some(&digest) {
            return Err(ApiError::new(
                409,
                "RECONCILIATION_REQUIRED",
                "Complete a fresh sync under the current policy before reopening.",
            ));
        }
        self.confirm_admin(principal, &digest)?;
        self.set_phase(false, None, false)?;
        Ok(json!({"schema_version":1,"status":"ready"}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    question: String,
    mode: Option<String>,
    graph: Option<bool>,
    limit: Option<usize>,
    answer_endpoint: Option<String>,
    #[serde(default)]
    rerank: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Removal {
    source_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryEnrichment {
    collection: String,
    lane: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphReviewList {
    collection: String,
    limit: Option<usize>,
    after: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReviewDecision {
    Approve,
    Dismiss,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphReviewRequest {
    collection: String,
    proposal_id: String,
    decision: ReviewDecision,
    revision: String,
    fingerprint: String,
    policy_version: i64,
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Restore {
    backup_id: String,
}

fn request_json<T: serde::de::DeserializeOwned>(request: &mut tiny_http::Request) -> ApiResult<T> {
    let too_large = ApiError::new(
        413,
        "REQUEST_TOO_LARGE",
        "The request exceeds the shared API body limit.",
    );
    let invalid = ApiError::new(
        400,
        "REQUEST_JSON",
        "Provide a JSON object with the documented fields.",
    );
    if request.body_length().is_some_and(|len| len > MAX_BODY) {
        return Err(too_large);
    }
    let mut bytes = Vec::new();
    request
        .as_reader()
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid)?;
    if bytes.len() > MAX_BODY {
        return Err(too_large);
    }
    let value = strict_json(&bytes).map_err(|_| invalid)?;
    serde_json::from_value(value).map_err(|_| invalid)
}

fn route(service: &Arc<Service>, request: &mut tiny_http::Request) -> ApiResult<(u16, Value)> {
    use tiny_http::Method::{Get, Post};
    let path = request.url().to_owned();
    if request.method() == &Get && path == "/health" {
        return Ok((200, json!({"status":"ok"})));
    }
    let mut headers = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Authorization"));
    let header = headers
        .next()
        .filter(|_| headers.next().is_none())
        .map(|h| h.value.as_str());
    let principal = service.auth.authenticate(header)?;
    let response = match (request.method(), path.as_str()) {
        (&Post, "/v1/query") => service.query(&principal, request_json(request)?)?,
        (&Post, "/v1/relationship") => service.relationship(&principal, request_json(request)?)?,
        (&Post, "/v1/graph-review/list") => {
            service.graph_review_list(&principal, request_json(request)?)?
        }
        (&Post, "/v1/graph-review/decide") => {
            service.graph_review_decide(&principal, request_json(request)?)?
        }
        (&Get, "/v1/admin/status") => service.admin_status(&principal)?,
        (&Post, "/v1/admin/sync") => return Ok((202, service.start_sync(principal)?)),
        (&Post, "/v1/admin/retry-enrichment") => {
            service.retry_enrichment(&principal, request_json(request)?)?
        }
        (&Post, "/v1/admin/remove") => {
            service.remove(&principal, &request_json::<Removal>(request)?.source_id)?
        }
        (&Post, "/v1/admin/backup") => service.backup(&principal)?,
        (&Post, "/v1/admin/restore") => {
            service.restore(&principal, &request_json::<Restore>(request)?.backup_id)?
        }
        (&Post, "/v1/admin/reopen") => service.reopen(&principal)?,
        (&Get, path) if path.starts_with("/v1/evidence/") => {
            service.evidence(&principal, &path[13..])?
        }
        _ => {
            return Err(ApiError::new(
                404,
                "ROUTE_NOT_FOUND",
                "Use a documented shared API route.",
            ));
        }
    };
    Ok((200, response))
}

fn handle(service: &Arc<Service>, mut request: tiny_http::Request) {
    let (status, body, retry_after) = match route(service, &mut request) {
        Ok((status, body)) => (status, body, None),
        Err(ApiError(status, code, message, retry_after)) => {
            let mut body = json!({"error":{"code":code,"message":message}});
            if let Some(seconds) = retry_after {
                body["error"]["retryable"] = json!(true);
                body["error"]["retry_after_seconds"] = json!(seconds);
            }
            (status, body, retry_after)
        }
    };
    let mut response = tiny_http::Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes("Content-Type", "application/json")
                .expect("fixed header"),
        )
        .with_header(
            tiny_http::Header::from_bytes("Cache-Control", "no-store").expect("fixed header"),
        );
    if status == 401 {
        response.add_header(
            tiny_http::Header::from_bytes("WWW-Authenticate", "Bearer").expect("fixed header"),
        );
    }
    if let Some(seconds) = retry_after {
        response.add_header(
            tiny_http::Header::from_bytes("Retry-After", seconds.to_string())
                .expect("numeric header"),
        );
    }
    let _ = request.respond(response);
}

pub fn serve(profile: &Path, bind: SocketAddr) -> Result<()> {
    let service = Arc::new(Service::open(profile)?);
    ensure!(
        bind.ip().is_loopback() || service.allow_proxy_http,
        "Bind to loopback or explicitly configure allow_proxy_http for a trusted TLS reverse proxy on a private network."
    );
    let server = Arc::new(
        tiny_http::Server::http(bind)
            .map_err(|_| anyhow::anyhow!("Could not bind the shared HTTP server."))?,
    );
    eprintln!(
        "Rust RAG shared service listening on {}",
        server.server_addr()
    );
    // The public boundary is the TLS proxy. Bound active handlers even when
    // an authorized request is waiting on a model.
    thread::scope(|scope| {
        for _ in 0..8 {
            let (service, server) = (Arc::clone(&service), Arc::clone(&server));
            scope.spawn(move || {
                for request in server.incoming_requests() {
                    handle(&service, request);
                }
            });
        }
    });
    Ok(())
}

pub fn doctor(profile: &Path) -> Result<Value> {
    let service = Service::open(profile)?;
    let (policy, _) = service.policy().map_err(|error| anyhow::anyhow!(error.2))?;
    let (maintenance, _, pending) = service.phase().map_err(|error| anyhow::anyhow!(error.2))?;
    let mut result = json!({"schema_version":1,"status":"ready","profile":"shared","sqlite":rusqlite::version(),
        "collection_count":service.sources.len(),"policy_version":policy.version,"maintenance":maintenance,
        "restore_pending":pending,"backup_configured":service.backups.is_some(),
        "http_binding":if service.allow_proxy_http { "private TLS proxy required" } else { "loopback only" }});
    if let Some(status) = service.reranker_status {
        result["reranker"] = json!(status);
    }
    Ok(result)
}
