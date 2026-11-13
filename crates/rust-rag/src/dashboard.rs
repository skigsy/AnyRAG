//! Allowlisted operator signals for one existing personal index.

pub(crate) mod history;
mod live;
mod view;

use crate::{
    enrichment::{self, Lane, ProgressCounts},
    files, guide,
    store::{self, ReadOnlyError},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const READ_WAIT: Duration = Duration::from_millis(25);
const READ_BUDGET: Duration = Duration::from_millis(250);
const REVIEW_QUEUE_LIMIT: usize = 10;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadState {
    Available,
    Missing,
    Unavailable,
    UnsupportedJournal,
    UnsupportedSchema,
}

impl From<ReadOnlyError> for ReadState {
    fn from(error: ReadOnlyError) -> Self {
        match error {
            ReadOnlyError::Unavailable => Self::Unavailable,
            ReadOnlyError::UnsupportedJournal => Self::UnsupportedJournal,
            ReadOnlyError::UnsupportedSchema => Self::UnsupportedSchema,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CoverageState {
    Idle,
    Current,
    Behind,
}

#[derive(Serialize)]
struct Coverage {
    state: CoverageState,
    eligible: i64,
    completed: i64,
    pending: i64,
    scheduling: Option<ProgressCounts>,
}

#[derive(Serialize)]
struct Index {
    storage_version: i64,
    advanced_schema: bool,
    sources: i64,
    chunks: i64,
    vector: Option<Coverage>,
    graph: Option<Coverage>,
}

impl Index {
    fn next_scheduling_change(&self) -> Option<i64> {
        self.vector
            .iter()
            .chain(self.graph.iter())
            .filter_map(|coverage| coverage.scheduling.as_ref())
            .flat_map(|counts| [counts.next_retry_unix, counts.provider_paused_until_unix])
            .flatten()
            .min()
    }
}

#[derive(Serialize)]
struct Snapshot<'a> {
    schema_version: u32,
    observed_at_unix_seconds: u64,
    scope: &'static str,
    state: ReadState,
    build: Build,
    index: Option<&'a Index>,
    history: history::Summary,
}

#[derive(Serialize)]
struct Build {
    model_endpoint: bool,
    native_embeddings: bool,
}

#[derive(Deserialize)]
struct ReviewQueue {
    schema_version: u32,
    proposals: Vec<ReviewProposal>,
    has_more: bool,
}

#[derive(Deserialize)]
struct ReviewProposal {
    proposal_id: String,
    subject: String,
    predicate: String,
    object: String,
    quote: String,
    path: String,
    start: u64,
    end: u64,
}

fn read_sources(requested: &Path, source: Option<&str>) -> Result<String> {
    let state = files::state_directory(requested, false)?;
    let mut db = open_index_reader(&state).map_err(|_| anyhow::anyhow!("Index unavailable"))?;
    let db = db.transaction()?;
    match source {
        None => {
            let mut query =
                db.prepare("SELECT source_id,path FROM sources ORDER BY source_id LIMIT 51")?;
            let entries = query.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(view::sources(
                entries.collect::<rusqlite::Result<Vec<_>>>()?,
            ))
        }
        Some(id) => {
            let path: Option<String> = db
                .query_row("SELECT path FROM sources WHERE source_id=?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(path) = path else {
                return Ok(view::missing_source());
            };
            let page = store::page_sql(&db)?;
            let anchor = store::anchor_sql(&db)?;
            let mut query = db.prepare(&format!(
                "SELECT c.start,c.end,{page},{anchor},substr(p.text,1,1200) FROM chunks c
                 JOIN passages p ON p.rowid=c.row_id WHERE c.source_id=?1
                 ORDER BY c.row_id LIMIT 31"
            ))?;
            let entries = query.query_map([id], |row| {
                Ok(view::SourcePassage {
                    start: row.get(0)?,
                    end: row.get(1)?,
                    page: row.get(2)?,
                    anchor: row.get(3)?,
                    text: row.get(4)?,
                })
            })?;
            Ok(view::source_passages(
                &path,
                entries.collect::<rusqlite::Result<Vec<_>>>()?,
            ))
        }
    }
}

fn coverage(
    db: &Connection,
    lane: Lane,
    eligible: i64,
    scheduled: bool,
) -> Result<Option<Coverage>> {
    let sql = match lane {
        Lane::Vector => {
            "SELECT m.fingerprint, (SELECT count(*) FROM chunks c
             JOIN chunk_vectors v ON v.chunk_row=c.row_id
             WHERE v.fingerprint=m.fingerprint AND v.dimensions=m.dimensions)
             FROM vector_config m WHERE m.id=1"
        }
        Lane::Graph => {
            "SELECT m.fingerprint, (SELECT count(*) FROM chunks c
             JOIN graph_chunks g ON g.chunk_row=c.row_id WHERE g.fingerprint=m.fingerprint)
             FROM graph_config m WHERE m.id=1"
        }
    };
    let configured: Option<(String, i64)> = db
        .query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((fingerprint, completed)) = configured else {
        return Ok(None);
    };
    ensure!((0..=eligible).contains(&completed));
    let pending = eligible - completed;
    let scheduling = scheduled
        .then(|| enrichment::progress_counts(db, lane, &fingerprint))
        .transpose()?;
    Ok(Some(Coverage {
        state: if eligible == 0 {
            CoverageState::Idle
        } else if pending == 0 {
            CoverageState::Current
        } else {
            CoverageState::Behind
        },
        eligible,
        completed,
        pending,
        scheduling,
    }))
}

fn open_index_reader(state: &Path) -> std::result::Result<Connection, ReadState> {
    if state
        .join("personal.sqlite3")
        .symlink_metadata()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return Err(ReadState::Missing);
    }
    let mut db = store::open_read_only_with_timeout(state, READ_WAIT).map_err(ReadState::from)?;
    let deadline = Instant::now() + READ_BUDGET;
    db.progress_handler(1_000, Some(move || Instant::now() >= deadline))
        .map_err(|_| ReadState::Unavailable)?;
    Ok(db)
}

fn read_index(state: &Path, reuse_counts: bool) -> std::result::Result<Option<Index>, ReadState> {
    let mut db = open_index_reader(state)?;
    let mut read = || -> Result<Option<Index>> {
        let tx = db.transaction()?;
        if reuse_counts {
            // A real SQLite read still detects an exclusive writer or an
            // unavailable index. The filesystem stamp alone cannot do that.
            tx.query_row("SELECT id FROM settings WHERE id=1", [], |_| Ok(()))?;
            tx.commit()?;
            return Ok(None);
        }
        // Status shares the active counts used by the CLI. Project just the
        // numeric fields and schema capability, never the root or fingerprint.
        let status = store::status(&tx)?;
        let storage_version = status["storage_version"]
            .as_i64()
            .context("Missing index version")?;
        let sources = status["sources"]
            .as_i64()
            .context("Missing active source count")?;
        let chunks = status["chunks"]
            .as_i64()
            .context("Missing active chunk count")?;
        let advanced = status["graph"] == true;
        let scheduled = storage_version >= 3;
        let vector = advanced
            .then(|| coverage(&tx, Lane::Vector, chunks, scheduled))
            .transpose()?
            .flatten();
        let graph = advanced
            .then(|| coverage(&tx, Lane::Graph, chunks, scheduled))
            .transpose()?
            .flatten();
        tx.commit()?;
        Ok(Some(Index {
            storage_version,
            advanced_schema: advanced,
            sources,
            chunks,
            vector,
            graph,
        }))
    };
    read().map_err(|_| ReadState::Unavailable)
}

type EvidenceStamp = ([u8; 32], i64, SystemTime, u64);

struct CachedIndex {
    stamp: Option<EvidenceStamp>,
    sampled_at: i64,
    index: Index,
}

#[derive(Default)]
struct CoverageCache(Option<CachedIndex>);

impl CoverageCache {
    fn read(&mut self, state: &Path, now: i64) -> std::result::Result<&Index, ReadState> {
        // Do not open or close a raw index handle while SQLite has an open
        // connection here. On Unix a descriptor close can release file locks.
        let stamp = history::evidence_stamp(state).ok();
        let reuse = self.0.as_ref().is_some_and(|previous| {
            stamp.is_some()
                && previous.stamp == stamp
                && previous.sampled_at <= now
                && previous
                    .index
                    .next_scheduling_change()
                    .is_none_or(|next| now < next)
        });
        let mut read = read_index(state, reuse)?;
        let stamp =
            stamp.filter(|before| history::evidence_stamp(state).ok().as_ref() == Some(before));
        // A commit can race the cache check even though SQLite is readable.
        // Return fresh transactional counts without caching an ambiguous stamp.
        if reuse && stamp.is_none() {
            read = read_index(state, false)?;
        }
        if let Some(index) = read {
            self.0 = Some(CachedIndex {
                stamp,
                sampled_at: now,
                index,
            });
        }
        Ok(&self.0.as_ref().expect("index was read or reused").index)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn snapshot<'a>(requested: &Path, cache: &'a mut CoverageCache) -> Snapshot<'a> {
    let local = files::state_directory_if_present(requested)
        .map_err(|_| ReadState::Unavailable)
        .and_then(|state| state.ok_or(ReadState::Missing));
    let now = i64::try_from(unix_seconds()).unwrap_or(i64::MAX);
    let (state, index, history) =
        match local.and_then(|local| cache.read(&local, now).map(|index| (index, local))) {
            Ok((index, local)) => (ReadState::Available, Some(index), history::summary(&local)),
            Err(state) => (state, None, history::Summary::unavailable()),
        };
    let capabilities = guide::capabilities();
    Snapshot {
        schema_version: 1,
        observed_at_unix_seconds: unix_seconds(),
        scope: "one_local_personal_index",
        state,
        build: Build {
            model_endpoint: capabilities["compiled"]["model_endpoint"] == true,
            native_embeddings: capabilities["compiled"]["native_embeddings"] == true,
        },
        index,
        history,
    }
}

fn read_review_queue(requested: &Path) -> Result<ReviewQueue> {
    let state = files::state_directory_if_present(requested)?.context("Index missing.")?;
    let mut db =
        open_index_reader(&state).map_err(|_| anyhow::anyhow!("Review queue unavailable."))?;
    let tx = db.transaction()?;
    let queue: ReviewQueue =
        serde_json::from_value(crate::graph::review_queue(&tx, REVIEW_QUEUE_LIMIT)?)?;
    ensure!(queue.schema_version == 1 && queue.proposals.len() <= REVIEW_QUEUE_LIMIT);
    tx.commit()?;
    Ok(queue)
}

pub(crate) fn serve(requested: &Path, open: bool, show_review_queue: bool) -> Result<()> {
    live::serve(requested, open, show_review_queue)
}

pub(crate) fn history_control(requested: &Path, action: history::Action) -> Result<Value> {
    Ok(serde_json::to_value(history::control(requested, action)?)?)
}

pub(crate) fn health(requested: &Path, output: Option<&Path>) -> Result<Value> {
    let mut coverage = CoverageCache::default();
    let snapshot = snapshot(requested, &mut coverage);
    let Some(output) = output else {
        return Ok(serde_json::to_value(snapshot)?);
    };
    ensure!(
        output
            .extension()
            .is_some_and(|extension| extension == "html"),
        "Choose a new .html file in a private folder."
    );
    let content = view::render(&snapshot);
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = files::directory(parent).context("Choose a local report folder.")?;
    let name = output.file_name().context("Choose a new report file.")?;
    let mut file = tempfile::NamedTempFile::new_in(&parent)
        .context("Cannot create a report in the selected folder.")?;
    file.write_all(content.as_bytes())
        .context("Could not finish writing the HTML report.")?;
    file.persist_noclobber(parent.join(name))
        .map_err(|error| error.error)
        .context("Cannot save the HTML report. Choose a new file.")?;
    Ok(json!({ "schema_version": 1, "html_written": true, "state": snapshot.state }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_reads_an_older_index_without_creating_queue_state() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let state = files::state_directory(&scratch.path().join("state"), true)?;
        drop(store::open(&state, true)?);
        let database = state.join("personal.sqlite3");
        let before = std::fs::read(&database)?;
        let queue = read_review_queue(&state)?;
        assert!(queue.proposals.is_empty());
        assert!(!queue.has_more);
        assert_eq!(std::fs::read(&database)?, before);
        assert!(!state.join("personal.sqlite3-journal").exists());
        Ok(())
    }
}
