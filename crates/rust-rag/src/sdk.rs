use crate::{files, store};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// An owner-local evidence index. For user and collection access checks use the
/// authenticated service. This type never calls an external model.
pub struct Index {
    state: PathBuf,
    db: Connection,
}

impl Index {
    /// Open a private index, creating it if requested. This never upgrades state.
    pub fn open(state: &Path, create: bool) -> Result<Self> {
        let state = files::state_directory(state, create)?;
        let db = store::open(&state, create)?;
        Ok(Self { state, db })
    }

    /// Explicitly enable PDF, graph and vector storage, preserving a rollback
    /// copy when upgrading an existing corpus.
    pub fn enable_advanced(&mut self) -> Result<Option<PathBuf>> {
        store::upgrade(&mut self.db, &self.state)
    }

    /// Parse and update Markdown and text. Use the executable for its isolated PDF worker.
    pub fn ingest_folder(&mut self, root: &Path, sync: bool) -> Result<Value> {
        let root = files::directory(root)?;
        ensure!(
            !self.state.starts_with(&root) && !root.starts_with(&self.state),
            "Use separate folders for the sources and the index."
        );
        store::ingest(&mut self.db, &root, sync, false, false)
    }

    /// Embed active passages with a caller-provided adapter. The caller chooses
    /// the model, its destinations, and how to handle partial failures.
    #[cfg(feature = "model")]
    pub fn embed_with(&mut self, embedder: &dyn crate::Embedder, limit: usize) -> Result<Value> {
        store::require_enrichment(&self.db)?;
        crate::index_vectors(&mut self.db, embedder, limit, false)
    }

    /// Return the current counts and index format.
    pub fn status(&self) -> Result<Value> {
        store::status(&self.db)
    }

    /// Return candidate source passages. When using vector or hybrid search,
    /// pass vectors in the same space used to build the index.
    pub fn query(
        &mut self,
        question: &str,
        limit: usize,
        graph: bool,
        mode: store::Mode,
        query_embedding: Option<(&str, &[f32])>,
    ) -> Result<Value> {
        store::query(
            &mut self.db,
            question,
            limit,
            graph,
            mode,
            query_embedding.map(|(fingerprint, values)| store::VectorQuery {
                fingerprint,
                values,
            }),
        )
    }

    /// Remove indexed evidence for a single source. Source files are untouched.
    pub fn remove(&mut self, source_id: &str) -> Result<usize> {
        store::remove(&mut self.db, Some(source_id))
    }
}
