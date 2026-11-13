//! Opt-in observations of personal CLI ingest and query operations.

mod spool;

use crate::{files, store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SIDECAR: &str = "telemetry.sqlite3";
const OPT_IN: &str = "telemetry.enabled";
const APP_ID: i32 = 0x5241_4454;
const VERSION: i32 = 1;
const RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_ROWS: i64 = 2_000;
const PAGE_SIZE: u64 = 4_096;
const MAX_PAGES: u64 = 512;
const MAX_DISK_BYTES: u64 = 8 * 1024 * 1024;
const BUSY_WAIT: Duration = Duration::from_millis(1);
const INGEST: i64 = 1;
const QUERY: i64 = 2;
const SCOPE: &str = "local_cli_ingest_and_query";

const SCHEMA: &str = "
CREATE TABLE binding (
    id INTEGER PRIMARY KEY CHECK(id=1),
    salt BLOB NOT NULL CHECK(length(salt)=16),
    index_key BLOB NOT NULL CHECK(length(index_key)=32),
    change_counter INTEGER NOT NULL
);
CREATE TABLE observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind INTEGER NOT NULL CHECK(kind IN (1,2)),
    observed_at INTEGER NOT NULL,
    nonce BLOB,
    payload TEXT NOT NULL CHECK(length(payload)<=1024),
    CHECK((kind=1 AND length(nonce)=16) OR (kind=2 AND nonce IS NULL))
);
CREATE INDEX observations_by_time ON observations(observed_at);";

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Action {
    Enable,
    Disable,
    Clear,
}

#[derive(Serialize)]
pub(crate) struct Summary {
    pub(crate) state: &'static str,
    pub(crate) scope: &'static str,
    pub(crate) collection_enabled: Option<bool>,
    pub(crate) reason: Option<&'static str>,
    pub(crate) recent_queries: Option<QuerySummary>,
    pub(crate) latest_ingest: Option<IngestObservation>,
    pub(crate) last_complete_reconciliation_unix_seconds: Option<i64>,
}

impl Summary {
    pub(super) fn unavailable() -> Self {
        Self::new("unavailable", None, Some("index_unavailable"))
    }

    fn new(state: &'static str, enabled: Option<bool>, reason: Option<&'static str>) -> Self {
        Self {
            state,
            scope: SCOPE,
            collection_enabled: enabled,
            reason,
            recent_queries: None,
            latest_ingest: None,
            last_complete_reconciliation_unix_seconds: None,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct QuerySummary {
    pub(crate) count: i64,
    pub(crate) first_observed_unix_seconds: i64,
    pub(crate) last_observed_unix_seconds: i64,
    pub(crate) median_duration_micros: i64,
    pub(crate) evidence: i64,
    pub(crate) weak_match: i64,
    pub(crate) abstained: i64,
    pub(crate) errors: i64,
    pub(crate) rerank_attempted: i64,
    pub(crate) rerank_fallbacks: Option<i64>,
    pub(crate) latest: QueryObservation,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryObservation {
    pub(crate) observed_at_unix_seconds: i64,
    pub(crate) status: QueryStatus,
    pub(crate) mode: QueryMode,
    pub(crate) graph_requested: bool,
    pub(crate) graph: Option<GraphObservation>,
    pub(crate) reranking: Option<RerankStatus>,
    pub(crate) duration_micros: i64,
    pub(crate) retrieval_micros: Option<i64>,
    pub(crate) rerank_micros: Option<i64>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueryStatus {
    Evidence,
    WeakMatch,
    Abstained,
    Error,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueryMode {
    Lexical,
    Vector,
    Hybrid,
}

impl From<store::Mode> for QueryMode {
    fn from(mode: store::Mode) -> Self {
        match mode {
            store::Mode::Lexical => Self::Lexical,
            store::Mode::Vector => Self::Vector,
            store::Mode::Hybrid => Self::Hybrid,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GraphObservation {
    pub(crate) seed: GraphSeed,
    pub(crate) index_pending: bool,
    pub(crate) construction_budget_exhausted: bool,
    pub(crate) traversal_budget_exhausted: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GraphSeed {
    Unavailable,
    NoSeed,
    Seeded,
    ResultLimit,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RerankStatus {
    Applied,
    Fallback,
    Skipped,
    PreservedIdentifierOrder,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngestObservation {
    pub(crate) started_unix_seconds: i64,
    pub(crate) finished_unix_seconds: Option<i64>,
    pub(crate) status: IngestStatus,
    pub(crate) duration_micros: Option<i64>,
    pub(crate) sync_requested: bool,
    pub(crate) inventory_complete: Option<bool>,
    pub(crate) sync_blocked: Option<bool>,
    pub(crate) sync_block_reason: Option<SyncBlockedReason>,
    pub(crate) counts: Option<IngestCounts>,
    pub(crate) complete_reconciliation: Option<bool>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IngestStatus {
    Completed,
    Partial,
    Error,
    CompletionUnobserved,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncBlockedReason {
    IncompleteInventory,
    ChangedDuringReconciliation,
    ReconciliationUnconfirmed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngestCounts {
    pub(crate) updated_sources: i64,
    pub(crate) unchanged_sources: i64,
    pub(crate) failed_sources: i64,
    pub(crate) deferred_sources: i64,
    pub(crate) retained_previous_sources: i64,
    pub(crate) removed_sources: i64,
    pub(crate) skipped_pdfs: i64,
    pub(crate) skipped_docx: i64,
    #[serde(default, skip_serializing_if = "zero_count")]
    pub(crate) skipped_xlsx: i64,
    pub(crate) skipped_links_or_placeholders: Option<i64>,
}

fn zero_count(value: &i64) -> bool {
    *value == 0
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn micros(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_micros()).unwrap_or(i64::MAX)
}

fn regular(path: &Path) -> Result<Option<Metadata>> {
    match path.symlink_metadata() {
        Ok(info) => {
            ensure!(info.is_file() && !files::link_or_unavailable(&info));
            Ok(Some(info))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn open_regular(path: &Path, writable: bool, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable).create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let info = file.metadata()?;
    ensure!(info.is_file() && !files::link_or_unavailable(&info));
    Ok(file)
}

fn header(path: &Path) -> Result<(File, [u8; 100])> {
    let mut file = open_regular(path, false, false)?;
    let header = read_header(&mut file)?;
    Ok((file, header))
}

fn read_header(file: &mut File) -> Result<[u8; 100]> {
    let mut header = [0; 100];
    file.read_exact(&mut header)?;
    ensure!(&header[..16] == b"SQLite format 3\0" && header[18..20] == [1, 1]);
    Ok(header)
}

#[cfg(unix)]
fn file_identity(file: &File, digest: &mut Sha256) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let info = file.metadata()?;
    digest.update(info.dev().to_le_bytes());
    digest.update(info.ino().to_le_bytes());
    if let Ok(created) = info
        .created()
        .and_then(|date| date.duration_since(UNIX_EPOCH).map_err(io::Error::other))
    {
        digest.update(created.as_nanos().to_le_bytes());
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn file_identity(file: &File, digest: &mut Sha256) -> Result<()> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
    // The file handle stays open and the API writes one FILE_ID_INFO on success.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            id.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    ensure!(
        success != 0,
        "Cannot read the index file identity: {}",
        io::Error::last_os_error()
    );
    let id = unsafe { id.assume_init() };
    digest.update(id.VolumeSerialNumber.to_le_bytes());
    digest.update(id.FileId.Identifier);
    digest.update(file.metadata()?.creation_time().to_le_bytes());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_: &File, _: &mut Sha256) -> Result<()> {
    anyhow::bail!("Index file identity is unavailable on this platform.")
}

fn stamp(state: &Path, salt: &[u8]) -> Result<([u8; 32], i64)> {
    let (file, header) = header(&state.join("personal.sqlite3"))?;
    identity_stamp(&file, &header, salt)
}

fn identity_stamp(file: &File, header: &[u8; 100], salt: &[u8]) -> Result<([u8; 32], i64)> {
    let change = &header[24..28];
    // An inconsistent header can be observed during a concurrent commit.
    ensure!(change == &header[92..96]);
    let mut digest = Sha256::new();
    digest.update(salt);
    file_identity(file, &mut digest)?;
    Ok((
        digest.finalize().into(),
        i64::from(u32::from_be_bytes(change.try_into()?)),
    ))
}

pub(super) fn evidence_stamp(state: &Path) -> Result<([u8; 32], i64, SystemTime, u64)> {
    let (file, header) = header(&state.join("personal.sqlite3"))?;
    let (key, counter) = identity_stamp(&file, &header, b"dashboard-coverage-cache-v1")?;
    let info = file.metadata()?;
    Ok((key, counter, info.modified()?, info.len()))
}

struct Binding {
    salt: Vec<u8>,
    index_key: Vec<u8>,
    change_counter: i64,
}

fn binding(db: &Connection) -> Result<Binding> {
    Ok(db.query_row(
        "SELECT salt,index_key,change_counter FROM binding WHERE id=1",
        [],
        |row| {
            Ok(Binding {
                salt: row.get(0)?,
                index_key: row.get(1)?,
                change_counter: row.get(2)?,
            })
        },
    )?)
}

fn current(binding: &Binding, state: &Path) -> Result<Option<i64>> {
    let (key, counter) = stamp(state, &binding.salt)?;
    Ok(
        (key.as_slice() == binding.index_key && counter >= binding.change_counter)
            .then_some(counter),
    )
}

fn connection(path: &Path, writable: bool) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_NOFOLLOW
        | if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
    let db = Connection::open_with_flags(path, flags)?;
    db.busy_timeout(BUSY_WAIT)?;
    db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;")?;
    if !writable {
        db.execute_batch("PRAGMA query_only=ON;")?;
    }
    Ok(db)
}

fn verify_size(state: &Path, main: u64) -> Result<()> {
    ensure!(main <= MAX_PAGES * PAGE_SIZE);
    let journal = regular(&state.join("telemetry.sqlite3-journal"))?.map_or(0, |info| info.len());
    ensure!(
        main.saturating_add(journal)
            .saturating_add(spool::stored_bytes(state)?)
            <= MAX_DISK_BYTES
    );
    ensure!(
        regular(&state.join("telemetry.sqlite3-wal"))?.is_none()
            && regular(&state.join("telemetry.sqlite3-shm"))?.is_none()
    );
    Ok(())
}

fn configure_writer(db: &Connection) -> Result<()> {
    db.pragma_update(None, "max_page_count", MAX_PAGES as i64)?;
    db.execute_batch(
        "PRAGMA synchronous=NORMAL; PRAGMA secure_delete=ON; PRAGMA cache_spill=OFF;",
    )?;
    Ok(())
}

fn sidecar_file(state: &Path) -> Result<Option<(File, [u8; 100])>> {
    let path = state.join(SIDECAR);
    let Some(info) = regular(&path)? else {
        return Ok(None);
    };
    verify_size(state, info.len())?;
    let (file, raw) = header(&path)?;
    validate_sidecar_header(&raw)?;
    Ok(Some((file, raw)))
}

fn validate_sidecar_header(raw: &[u8; 100]) -> Result<()> {
    ensure!(u64::from(u16::from_be_bytes([raw[16], raw[17]])) == PAGE_SIZE);
    ensure!(i32::from_be_bytes(raw[68..72].try_into()?) == APP_ID);
    ensure!(i32::from_be_bytes(raw[60..64].try_into()?) == VERSION);
    Ok(())
}

#[derive(PartialEq, Eq)]
struct SidecarStamp {
    identity: [u8; 32],
    change_counter: i64,
    modified: SystemTime,
    length: u64,
}

fn sidecar_stamp(file: &File, header: [u8; 100]) -> Result<SidecarStamp> {
    validate_sidecar_header(&header)?;
    let (identity, change_counter) =
        identity_stamp(file, &header, b"dashboard-history-sidecar-v1")?;
    let info = file.metadata()?;
    ensure!(info.len() <= MAX_PAGES * PAGE_SIZE);
    Ok(SidecarStamp {
        identity,
        change_counter,
        modified: info.modified()?,
        length: info.len(),
    })
}

fn open(state: &Path, writable: bool) -> Result<Option<Connection>> {
    if sidecar_file(state)?.is_none() {
        return Ok(None);
    }
    open_sidecar_connection(state, writable).map(Some)
}

fn open_sidecar_connection(state: &Path, writable: bool) -> Result<Connection> {
    let db = connection(&state.join(SIDECAR), writable)?;
    let mode: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    ensure!(mode == "delete");
    let app_id: i32 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(app_id == APP_ID && version == VERSION);
    if writable {
        configure_writer(&db)?;
    }
    Ok(db)
}

fn bind(db: &Connection, state: &Path) -> Result<()> {
    let salt: Vec<u8> = db.query_row("SELECT randomblob(16)", [], |row| row.get(0))?;
    let (key, counter) = stamp(state, &salt)?;
    db.execute(
        "INSERT OR REPLACE INTO binding VALUES (1,?1,?2,?3)",
        params![salt, key.as_slice(), counter],
    )?;
    Ok(())
}

fn create(state: &Path) -> Result<()> {
    verify_size(state, 0)?;
    ensure!(regular(&state.join("telemetry.sqlite3-journal"))?.is_none());
    let file = tempfile::NamedTempFile::new_in(state)?;
    let mut db = connection(file.path(), true).context("Cannot open the new history database")?;
    db.pragma_update(None, "page_size", PAGE_SIZE as i64)?;
    configure_writer(&db)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "application_id", APP_ID)?;
    tx.pragma_update(None, "user_version", VERSION)?;
    bind(&tx, state).context("Cannot bind history to the index")?;
    tx.commit()
        .context("Cannot commit the new history database")?;
    drop(db);
    file.persist_noclobber(state.join(SIDECAR))
        .map_err(|error| error.error)
        .context("Cannot install the new history database")?;
    Ok(())
}

fn opt_in(state: &Path) -> Result<bool> {
    match regular(&state.join(OPT_IN))? {
        Some(info) => {
            ensure!(info.len() == 0);
            Ok(true)
        }
        None => Ok(false),
    }
}

fn prune(tx: &Transaction<'_>, time: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM observations WHERE observed_at < ?1 OR observed_at > ?2",
        params![time.saturating_sub(RETENTION_SECONDS), time],
    )?;
    tx.execute(
        "DELETE FROM observations WHERE id <= (SELECT max(id)-?1 FROM observations)",
        [MAX_ROWS],
    )?;
    Ok(())
}

fn write_event<T>(
    db: &mut Connection,
    state: &Path,
    expected: Option<(&[u8], i64)>,
    update: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<(T, Vec<u8>, i64)> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(opt_in(state)?);
    let bound = binding(&tx)?;
    if let Some((salt, _)) = expected {
        ensure!(bound.salt == salt);
    }
    let counter = current(&bound, state)?.context("The index has changed.")?;
    tx.execute(
        "UPDATE binding SET change_counter=?1 WHERE id=1 AND change_counter<?1",
        [counter],
    )?;
    let result = update(&tx)?;
    let at = now();
    // Completion only updates a row in place. The start already enforced
    // both retention bounds at this cutoff, as did any intervening insert.
    if !expected.is_some_and(|(_, last_pruned_at)| last_pruned_at == at) {
        prune(&tx, at)?;
    }
    tx.commit()?;
    Ok((result, bound.salt, at))
}

pub(crate) fn control(requested: &Path, action: Action) -> Result<Summary> {
    let state = files::state_directory(requested, false)
        .context("Choose the private state directory for an existing personal index.")?;
    let action_result = (|| -> Result<()> {
        if matches!(action, Action::Enable) {
            store::open_read_only_with_timeout(&state, BUSY_WAIT)
                .map_err(|_| anyhow::anyhow!("The personal index is unavailable."))?;
        }
        let mut queries = match spool::Writer::lock(&state, matches!(action, Action::Enable)) {
            Ok(writer) => writer,
            // A broken optional spool must never prevent stopping collection.
            // A live writer, on the other hand, must finish before opt-out.
            Err(error) if matches!(action, Action::Disable) && !spool::busy(&error) => None,
            Err(error) => return Err(error),
        };
        let enabled = opt_in(&state).context("Cannot read the history setting.")?;
        match action {
            Action::Enable => {
                let queries = queries
                    .as_mut()
                    .context("Query history storage is missing.")?;
                let bound = if let Some(mut db) = open(&state, true)? {
                    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let bound = binding(&tx)?;
                    if current(&bound, &state)?.is_none() || queries.rewound(&state, &bound)? {
                        tx.execute("DELETE FROM observations", [])?;
                        bind(&tx, &state)?;
                    }
                    prune(&tx, now())?;
                    let bound = binding(&tx)?;
                    tx.commit()?;
                    bound
                } else {
                    create(&state)?;
                    binding(&open(&state, false)?.context("History storage is missing.")?)?
                };
                queries.ensure_binding(&bound)?;
                if !enabled {
                    tempfile::NamedTempFile::new_in(&state)?
                        .persist_noclobber(state.join(OPT_IN))
                        .map_err(|error| error.error)
                        .context("Cannot install the opt-in marker")?;
                }
            }
            Action::Disable if enabled => {
                // Serialize with active recorders when the sidecar is available.
                // Opt-out also works if the optional sidecar is unreadable.
                let db = open(&state, true).ok().flatten();
                if let Some(mut db) = db {
                    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    fs::remove_file(state.join(OPT_IN))?;
                    tx.commit()?;
                } else {
                    fs::remove_file(state.join(OPT_IN))?;
                }
            }
            Action::Clear => {
                if let Some(mut db) = open(&state, true)? {
                    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    tx.execute("DELETE FROM observations", [])?;
                    if let Some(queries) = queries.as_mut() {
                        queries.clear(Some(&binding(&tx)?))?;
                    }
                    tx.commit()?;
                } else if let Some(queries) = queries.as_mut() {
                    queries.clear(None)?;
                }
            }
            Action::Disable => {}
        }
        Ok(())
    })();
    action_result.map_err(|_| {
        anyhow::anyhow!(
            "Could not update local history. Check the index and history storage, then retry."
        )
    })?;
    Ok(summary(&state))
}

fn summarize_queries(mut queries: Vec<QueryObservation>) -> Option<QuerySummary> {
    let latest = queries.pop()?;
    let mut first = latest.observed_at_unix_seconds;
    let mut last = first;
    let mut timings = Vec::with_capacity(queries.len() + 1);
    let mut evidence = 0;
    let mut weak_match = 0;
    let mut abstained = 0;
    let mut errors = 0;
    let mut rerank_attempted = 0;
    let mut rerank_fallbacks = 0;
    for item in queries.iter().chain(std::iter::once(&latest)) {
        first = first.min(item.observed_at_unix_seconds);
        last = last.max(item.observed_at_unix_seconds);
        timings.push(item.duration_micros);
        match item.status {
            QueryStatus::Evidence => evidence += 1,
            QueryStatus::WeakMatch => weak_match += 1,
            QueryStatus::Abstained => abstained += 1,
            QueryStatus::Error => errors += 1,
        }
        match item.reranking {
            Some(RerankStatus::Applied) => rerank_attempted += 1,
            Some(RerankStatus::Fallback) => {
                rerank_attempted += 1;
                rerank_fallbacks += 1;
            }
            _ => {}
        }
    }
    timings.sort_unstable();
    let middle = timings.len() / 2;
    let median = if timings.len() % 2 == 0 {
        timings[middle - 1].midpoint(timings[middle])
    } else {
        timings[middle]
    };
    Some(QuerySummary {
        count: timings.len() as i64,
        first_observed_unix_seconds: first,
        last_observed_unix_seconds: last,
        median_duration_micros: median,
        evidence,
        weak_match,
        abstained,
        errors,
        rerank_attempted,
        rerank_fallbacks: (rerank_attempted > 0).then_some(rerank_fallbacks),
        latest,
    })
}

fn read_summary(state: &Path, enabled: bool) -> Result<Summary> {
    let Some(mut db) = open(state, false)? else {
        return Ok(if enabled {
            Summary::new("unavailable", Some(true), Some("storage_missing"))
        } else {
            Summary::new("disabled", Some(false), None)
        });
    };
    let time = now();
    let oldest = time.saturating_sub(RETENTION_SECONDS);
    // Keep the SQLite read lock only long enough to copy the bounded rows.
    let tx = db.transaction()?;
    let bound = binding(&tx)?;
    let rows = {
        let mut select = tx.prepare(
            "SELECT kind,observed_at,payload FROM observations WHERE id IN (
                 SELECT id FROM observations WHERE observed_at BETWEEN ?1 AND ?2
                 ORDER BY id DESC LIMIT ?3
             ) ORDER BY id",
        )?;
        let rows = select.query_map(params![oldest, time, MAX_ROWS], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.commit()?;
    drop(db);
    let index_changed = || {
        Summary::new(
            "unavailable",
            Some(enabled),
            Some("index_replaced_or_rolled_back"),
        )
    };
    if current(&bound, state)?.is_none() {
        return Ok(index_changed());
    }
    let Some(query_frames) = spool::snapshot(state, &bound, enabled)? else {
        return Ok(index_changed());
    };

    enum Observation {
        Query(QueryObservation),
        Ingest(IngestObservation),
    }
    let mut merged = Vec::with_capacity(rows.len() + query_frames.len());
    for (kind, observed_at, payload) in rows {
        ensure!(payload.len() <= 1024);
        let observation = match kind {
            QUERY => Observation::Query(serde_json::from_str(&payload)?),
            INGEST => Observation::Ingest(serde_json::from_str(&payload)?),
            _ => anyhow::bail!("Unsupported observation kind"),
        };
        merged.push((observed_at, observation));
    }
    merged.extend(query_frames.into_iter().filter_map(|item| {
        let at = item.observed_at_unix_seconds;
        (oldest..=time)
            .contains(&at)
            .then_some((at, Observation::Query(item)))
    }));
    merged.sort_by_key(|(at, _)| *at);
    let excess = merged.len().saturating_sub(MAX_ROWS as usize);
    let mut summary = Summary::new(
        if enabled { "idle" } else { "disabled" },
        Some(enabled),
        None,
    );
    let mut queries = Vec::new();
    for (_, observation) in merged.into_iter().skip(excess) {
        match observation {
            Observation::Query(item) => queries.push(item),
            Observation::Ingest(item) => {
                if item.complete_reconciliation == Some(true) {
                    summary.last_complete_reconciliation_unix_seconds = summary
                        .last_complete_reconciliation_unix_seconds
                        .max(item.finished_unix_seconds);
                }
                summary.latest_ingest = Some(item);
            }
        }
    }
    summary.recent_queries = summarize_queries(queries);
    if enabled && (summary.recent_queries.is_some() || summary.latest_ingest.is_some()) {
        summary.state = "available";
    }
    Ok(summary)
}

pub(crate) fn summary(requested: &Path) -> Summary {
    let state = match files::state_directory_if_present(requested) {
        Ok(Some(state)) => state,
        Ok(None) => return Summary::new("unavailable", None, Some("index_missing")),
        Err(_) => return Summary::new("unavailable", None, Some("state_unavailable")),
    };
    let Ok(enabled) = opt_in(&state) else {
        return Summary::new("unavailable", None, Some("setting_unavailable"));
    };
    read_summary(&state, enabled)
        .unwrap_or_else(|_| Summary::new("unavailable", Some(enabled), Some("storage_unavailable")))
}

pub(crate) struct IngestRecorder<'a> {
    state: &'a Path,
    db: Connection,
    sidecar: SidecarStamp,
    salt: Vec<u8>,
    pruned_at: i64,
    id: i64,
    nonce: Vec<u8>,
    start: Instant,
    observation: IngestObservation,
}

impl<'a> IngestRecorder<'a> {
    pub(crate) fn start(state: &'a Path, sync_requested: bool) -> Option<Self> {
        if !opt_in(state).unwrap_or(false) {
            return None;
        }
        let (mut file, _) = sidecar_file(state).ok()??;
        let mut db = open_sidecar_connection(state, true).ok()?;
        let at = now();
        let observation = IngestObservation {
            started_unix_seconds: at,
            finished_unix_seconds: None,
            status: IngestStatus::CompletionUnobserved,
            duration_micros: None,
            sync_requested,
            inventory_complete: None,
            sync_blocked: None,
            sync_block_reason: None,
            counts: None,
            complete_reconciliation: None,
        };
        let ((id, nonce), salt, pruned_at) = write_event(&mut db, state, None, |tx| {
            let payload = serde_json::to_string(&observation)?;
            Ok(tx.query_row(
                "INSERT INTO observations(kind,observed_at,nonce,payload)
                 VALUES (?1,?2,randomblob(16),?3) RETURNING id,nonce",
                params![INGEST, at, payload],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .ok()?;
        // No SQLite statement or transaction is alive while the raw file is
        // read or closed. Closing it inside a transaction could release POSIX
        // locks held by SQLite on this file.
        file.rewind().ok()?;
        let header = read_header(&mut file).ok()?;
        let sidecar = sidecar_stamp(&file, header).ok()?;
        Some(Self {
            state,
            db,
            sidecar,
            salt,
            pruned_at,
            id,
            nonce,
            start: Instant::now(),
            observation,
        })
    }

    pub(crate) fn finish(mut self, result: &Result<Value>) {
        self.observation.finished_unix_seconds = Some(now());
        self.observation.duration_micros = Some(micros(self.start));
        if let Ok(report) = result {
            if self.observation.complete(report).is_none() {
                return;
            }
        } else {
            self.observation.status = IngestStatus::Error;
            self.observation.complete_reconciliation =
                self.observation.sync_requested.then_some(false);
        }
        let _ = (|| -> Result<()> {
            let (file, header) =
                sidecar_file(self.state)?.context("History storage is missing.")?;
            let observed = sidecar_stamp(&file, header)?;
            drop(file);
            ensure!(observed.identity == self.sidecar.identity);
            if observed != self.sidecar {
                // A concurrent control or ingest may have written this file.
                // Require a later commit and a fresh page cache before using
                // its rows. Refuse rewinds and changes outside SQLite.
                ensure!(observed.change_counter > self.sidecar.change_counter);
                self.db = open_sidecar_connection(self.state, true)?;
            }
            write_event(
                &mut self.db,
                self.state,
                Some((&self.salt, self.pruned_at)),
                |tx| {
                    tx.execute(
                        "UPDATE observations SET payload=?1 WHERE id=?2 AND nonce=?3",
                        params![
                            serde_json::to_string(&self.observation)?,
                            self.id,
                            self.nonce
                        ],
                    )?;
                    Ok(())
                },
            )?;
            Ok(())
        })();
    }
}

impl IngestObservation {
    fn complete(&mut self, report: &Value) -> Option<()> {
        let failures = report["failures"].as_array()?;
        let complete = report["inventory_complete"].as_bool()?;
        let blocked = report["sync_blocked"].as_bool()?;
        let deferred = report["deferred_sources"].as_i64()?;
        let partial = !complete || blocked || !failures.is_empty() || deferred > 0;
        self.inventory_complete = Some(complete);
        self.sync_blocked = self.sync_requested.then_some(blocked);
        self.sync_block_reason =
            (self.sync_requested && blocked).then(|| match report["sync_reason"].as_str() {
                _ if !complete => SyncBlockedReason::IncompleteInventory,
                Some("The fresh inventory contains a link or unavailable entry.") => {
                    SyncBlockedReason::IncompleteInventory
                }
                Some(
                    "An indexed path reappeared after the fresh inventory."
                    | "An indexed path changed during removal confirmation."
                    | "The source root changed during removal confirmation.",
                ) => SyncBlockedReason::ChangedDuringReconciliation,
                _ => SyncBlockedReason::ReconciliationUnconfirmed,
            });
        self.complete_reconciliation = self.sync_requested.then_some(!partial);
        self.status = if partial {
            IngestStatus::Partial
        } else {
            IngestStatus::Completed
        };
        self.counts = Some(IngestCounts {
            updated_sources: report["updated_sources"].as_i64()?,
            unchanged_sources: report["unchanged_sources"].as_i64()?,
            failed_sources: failures
                .iter()
                .filter(|row| row["stage"] != "inventory")
                .count() as i64,
            deferred_sources: deferred,
            retained_previous_sources: failures
                .iter()
                .filter(|row| row["retained_previous"] == true)
                .count() as i64,
            removed_sources: report["removed_sources"].as_i64()?,
            skipped_pdfs: report["skipped_pdfs"].as_i64()?,
            skipped_docx: report["skipped_docx"].as_i64()?,
            skipped_xlsx: report["skipped_xlsx"].as_i64().unwrap_or_default(),
            skipped_links_or_placeholders: report["skipped_links_or_placeholders"].as_i64(),
        });
        Some(())
    }
}

pub(crate) struct QueryRecorder<'a> {
    state: &'a Path,
    start: Instant,
    mode: QueryMode,
    graph_requested: bool,
    retrieval_micros: Option<i64>,
    rerank_micros: Option<i64>,
}

impl<'a> QueryRecorder<'a> {
    pub(crate) fn start(state: &'a Path, mode: store::Mode, graph_requested: bool) -> Option<Self> {
        opt_in(state).unwrap_or(false).then(|| Self {
            state,
            start: Instant::now(),
            mode: mode.into(),
            graph_requested,
            retrieval_micros: None,
            rerank_micros: None,
        })
    }

    pub(crate) fn retrieval_finished(&mut self, started: Instant) {
        self.retrieval_micros = Some(micros(started));
    }

    #[cfg(feature = "inference")]
    pub(crate) fn rerank_finished(&mut self, started: Instant) {
        self.rerank_micros = Some(micros(started));
    }

    pub(crate) fn finish(self, result: &Result<Value>) {
        let (status, graph, reranking) = match result {
            Ok(report) => {
                let status = match report["status"].as_str() {
                    Some("evidence") => QueryStatus::Evidence,
                    Some("weak_match") => QueryStatus::WeakMatch,
                    Some("abstained") => QueryStatus::Abstained,
                    _ => return,
                };
                (
                    status,
                    graph_observation(&report["graph_coverage"]),
                    rerank_observation(&report["reranking"]),
                )
            }
            Err(_) => (QueryStatus::Error, None, None),
        };
        let item = QueryObservation {
            observed_at_unix_seconds: now(),
            status,
            mode: self.mode,
            graph_requested: self.graph_requested,
            graph,
            reranking,
            duration_micros: micros(self.start),
            retrieval_micros: self.retrieval_micros,
            rerank_micros: self.rerank_micros,
        };
        let _ = spool::append(self.state, &item);
    }
}

fn graph_observation(graph: &Value) -> Option<GraphObservation> {
    let seed = match graph["seed_status"].as_str()? {
        "graph_not_configured" => GraphSeed::Unavailable,
        "no_strong_seed" | "no_seed_assertions" => GraphSeed::NoSeed,
        "seeded" => GraphSeed::Seeded,
        "result_limit" => GraphSeed::ResultLimit,
        _ => return None,
    };
    Some(GraphObservation {
        seed,
        index_pending: graph["index_pending"].as_bool()?,
        construction_budget_exhausted: graph["construction_budget_exhausted"].as_bool()?,
        traversal_budget_exhausted: graph["traversal_budget_exhausted"].as_bool()?,
    })
}

fn rerank_observation(reranking: &Value) -> Option<RerankStatus> {
    match reranking["status"].as_str()? {
        "applied" if reranking["reason"] == "preserved_identifier_order" => {
            Some(RerankStatus::PreservedIdentifierOrder)
        }
        "applied" => Some(RerankStatus::Applied),
        "fallback" => Some(RerankStatus::Fallback),
        "skipped" => Some(RerankStatus::Skipped),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn private_state_dir() -> io::Result<tempfile::TempDir> {
        let mut fixture = tempfile::Builder::new();
        fixture.prefix("anyrag-history-state-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fixture.permissions(fs::Permissions::from_mode(0o700));
        }
        fixture.tempdir()
    }

    #[test]
    fn ingest_markers_preserve_completion_and_reconciliation_scope() -> Result<()> {
        let state = private_state_dir()?;
        let source = tempfile::tempdir()?;
        let mut db = store::open(state.path(), true)?;
        assert_eq!(summary(state.path()).state, "disabled");
        assert!(!state.path().join(SIDECAR).exists());
        assert_eq!(control(state.path(), Action::Enable)?.state, "idle");

        // Dropping the recorder simulates the absence of a completion callback.
        drop(IngestRecorder::start(state.path(), true).context("Missing marker")?);
        let report = summary(state.path());
        assert!(matches!(
            report.latest_ingest.unwrap().status,
            IngestStatus::CompletionUnobserved
        ));
        assert!(report.last_complete_reconciliation_unix_seconds.is_none());

        fs::write(source.path().join("broken.md"), [0xff, 0xff])?;
        let recorder = IngestRecorder::start(state.path(), true).context("Missing marker")?;
        let observer = open(state.path(), false)?.context("History storage is missing.")?;
        let id = recorder.id;
        let nonce = recorder.nonce.clone();
        let read_marker = || -> Result<IngestObservation> {
            let payload: String = observer.query_row(
                "SELECT payload FROM observations WHERE id=?1 AND nonce=?2",
                params![id, nonce],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str(&payload)?)
        };
        assert!(matches!(
            read_marker()?.status,
            IngestStatus::CompletionUnobserved
        ));
        let result = store::ingest(&mut db, source.path(), true, false, false);
        assert_eq!(result.as_ref().unwrap()["inventory_complete"], true);
        assert_eq!(result.as_ref().unwrap()["sync_blocked"], false);
        recorder.finish(&result);
        assert!(matches!(read_marker()?.status, IngestStatus::Partial));
        let report = summary(state.path());
        let ingest = report.latest_ingest.unwrap();
        assert!(matches!(ingest.status, IngestStatus::Partial));
        assert_eq!(ingest.counts.unwrap().failed_sources, 1);
        assert!(report.last_complete_reconciliation_unix_seconds.is_none());

        fs::write(
            source.path().join("broken.md"),
            "The workshop contains PRIVATE-PASSAGE-CANARY.",
        )?;
        let recorder = IngestRecorder::start(state.path(), true).context("Missing marker")?;
        let result = store::ingest(&mut db, source.path(), true, false, false);
        recorder.finish(&result);
        let report = summary(state.path());
        assert_eq!(
            report.latest_ingest.unwrap().complete_reconciliation,
            Some(true)
        );
        assert!(report.last_complete_reconciliation_unix_seconds.is_some());

        let result = store::query(&mut db, "workshop", 3, false, store::Mode::Lexical, None);
        QueryRecorder::start(state.path(), store::Mode::Lexical, false)
            .context("Expected enabled capture")?
            .finish(&result);
        let report = summary(state.path()).recent_queries.unwrap();
        assert_eq!((report.count, report.evidence), (1, 1));
        assert_eq!(report.rerank_fallbacks, None);
        assert!(
            !fs::read(state.path().join(SIDECAR))?
                .windows(b"PRIVATE-PASSAGE-CANARY".len())
                .any(|bytes| bytes == b"PRIVATE-PASSAGE-CANARY")
        );
        Ok(())
    }

    #[test]
    fn live_ingest_recorder_rechecks_control_changes_and_nonce() -> Result<()> {
        let state = private_state_dir()?;
        let _index = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        let first = IngestRecorder::start(state.path(), false).context("Missing first marker")?;
        let second = IngestRecorder::start(state.path(), false).context("Missing second marker")?;
        let first_id = first.id;
        let failed = Err(anyhow::anyhow!("synthetic ingest failure"));
        first.finish(&failed);
        let observer = open(state.path(), false)?.unwrap();
        let first_payload: String = observer.query_row(
            "SELECT payload FROM observations WHERE id=?1",
            [first_id],
            |row| row.get(0),
        )?;
        let first_completed: IngestObservation = serde_json::from_str(&first_payload)?;
        assert!(matches!(first_completed.status, IngestStatus::Error));

        control(state.path(), Action::Clear)?;
        let third = IngestRecorder::start(state.path(), false).context("Missing third marker")?;
        second.finish(&failed);
        assert!(matches!(
            summary(state.path()).latest_ingest.unwrap().status,
            IngestStatus::CompletionUnobserved
        ));
        control(state.path(), Action::Disable)?;
        third.finish(&failed);
        let disabled = summary(state.path());
        assert_eq!(disabled.state, "disabled");
        assert!(matches!(
            disabled.latest_ingest.unwrap().status,
            IngestStatus::CompletionUnobserved
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn live_ingest_recorder_does_not_modify_replaced_sidecar() -> Result<()> {
        let state = private_state_dir()?;
        let _index = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        let recorder = IngestRecorder::start(state.path(), true).context("Missing marker")?;
        let path = state.path().join(SIDECAR);
        let displaced = state.path().join("displaced.sqlite3");
        let before = fs::read(&path)?;
        fs::rename(&path, &displaced)?;
        fs::write(&path, &before)?;
        recorder.finish(&Err(anyhow::anyhow!("synthetic ingest failure")));
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(fs::read(&displaced)?, before);
        Ok(())
    }

    #[test]
    fn locked_history_fails_closed_and_can_be_cleared_without_evidence() -> Result<()> {
        let state = private_state_dir()?;
        let db = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        let telemetry = open(state.path(), true)?.unwrap();
        let lock = spool::Writer::lock(state.path(), false)?.unwrap();
        let recorded = Ok(json!({"status": "abstained", "question": "PRIVATE-QUERY-CANARY"}));
        QueryRecorder::start(state.path(), store::Mode::Lexical, false)
            .unwrap()
            .finish(&recorded);
        drop(lock);
        assert_eq!(summary(state.path()).state, "idle");
        QueryRecorder::start(state.path(), store::Mode::Lexical, false)
            .unwrap()
            .finish(&recorded);
        assert_eq!(summary(state.path()).recent_queries.unwrap().abstained, 1);
        assert_eq!(control(state.path(), Action::Disable)?.state, "disabled");
        assert!(QueryRecorder::start(state.path(), store::Mode::Lexical, false).is_none());
        drop(db);
        fs::remove_file(state.path().join("personal.sqlite3"))?;
        control(state.path(), Action::Clear)?;
        let rows: i64 =
            telemetry.query_row("SELECT count(*) FROM observations", [], |row| row.get(0))?;
        assert_eq!(rows, 0);
        Ok(())
    }

    #[test]
    fn replacement_or_rewind_hides_old_observations() -> Result<()> {
        let state = private_state_dir()?;
        let source = tempfile::tempdir()?;
        let index = state.path().join("personal.sqlite3");
        let mut db = store::open(state.path(), true)?;
        let earlier = fs::read(&index)?;
        control(state.path(), Action::Enable)?;
        fs::write(source.path().join("note.md"), "Synthetic passage")?;
        let recorder = IngestRecorder::start(state.path(), true).unwrap();
        recorder.finish(&store::ingest(&mut db, source.path(), true, false, false));
        assert_eq!(summary(state.path()).state, "available");

        drop(db);
        fs::write(&index, &earlier)?;
        let report = summary(state.path());
        assert_eq!(report.reason, Some("index_replaced_or_rolled_back"));
        assert!(report.latest_ingest.is_none());
        assert_eq!(control(state.path(), Action::Enable)?.state, "idle");

        // Rename the original aside to prevent the filesystem from recycling its ID.
        fs::rename(&index, state.path().join("displaced.sqlite3"))?;
        fs::write(&index, &earlier)?;
        assert_eq!(
            summary(state.path()).reason,
            Some("index_replaced_or_rolled_back")
        );
        Ok(())
    }

    #[test]
    fn retention_bounds_and_measured_zero_survive_summary() -> Result<()> {
        let state = private_state_dir()?;
        let _index = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        let at = now();
        let payload = serde_json::to_string(&QueryObservation {
            observed_at_unix_seconds: at,
            status: QueryStatus::Evidence,
            mode: QueryMode::Lexical,
            graph_requested: false,
            graph: None,
            reranking: Some(RerankStatus::Applied),
            duration_micros: 0,
            retrieval_micros: Some(0),
            rerank_micros: Some(0),
        })?;
        let mut writer = open(state.path(), true)?.unwrap();
        write_event(&mut writer, state.path(), None, |tx| {
            tx.execute(
                "WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n<?1)
                 INSERT INTO observations(kind,observed_at,payload) SELECT ?2,?3,?4 FROM seq",
                params![MAX_ROWS + 7, QUERY, at, payload],
            )?;
            tx.execute(
                "INSERT INTO observations(kind,observed_at,payload) VALUES(?1,?2,?3)",
                params![QUERY, at - RETENTION_SECONDS - 1, payload],
            )?;
            Ok(())
        })?;
        let report = summary(state.path()).recent_queries.unwrap();
        assert_eq!(report.count, MAX_ROWS);
        assert_eq!(report.median_duration_micros, 0);
        assert_eq!(report.rerank_attempted, MAX_ROWS);
        assert_eq!(report.rerank_fallbacks, Some(0));
        let db = open(state.path(), false)?.unwrap();
        let rows: i64 = db.query_row("SELECT count(*) FROM observations", [], |row| row.get(0))?;
        assert_eq!(rows, MAX_ROWS);
        verify_size(state.path(), state.path().join(SIDECAR).metadata()?.len())?;
        Ok(())
    }
}
