//! Fixed-size, best-effort query history. Ingest observations stay in SQLite.

use super::{
    Binding, GraphObservation, GraphSeed, MAX_ROWS, QueryMode, QueryObservation, QueryStatus,
    RerankStatus, current, open_regular, opt_in, regular,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const NAME: &str = "telemetry.queries";
const MAGIC: &[u8; 8] = b"RAGQv001";
const HEAD: usize = 80;
const FRAME: usize = 56;
// The spare slot keeps the preceding committed window intact until the next
// head is published, even when an append tears while the ring is full.
const SLOTS: usize = MAX_ROWS as usize + 1;
const SIZE: usize = 2 * HEAD + SLOTS * FRAME;
pub(super) const RESERVED_BYTES: u64 = SIZE as u64;

struct Head {
    sequence: u64,
    binding: Binding,
}

fn checksum(bytes: &[u8]) -> [u8; 8] {
    Sha256::digest(bytes)[..8].try_into().unwrap()
}

fn valid(bytes: &[u8]) -> bool {
    let (body, suffix) = bytes.split_at(bytes.len() - 8);
    checksum(body) == suffix
}

fn encode_head(sequence: u64, binding: &Binding, counter: i64) -> [u8; HEAD] {
    let mut bytes = [0; HEAD];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..16].copy_from_slice(&sequence.to_le_bytes());
    bytes[16..24].copy_from_slice(&counter.to_le_bytes());
    bytes[24..40].copy_from_slice(&binding.salt);
    bytes[40..72].copy_from_slice(&binding.index_key);
    let sum = checksum(&bytes[..72]);
    bytes[72..].copy_from_slice(&sum);
    bytes
}

fn decode_head(bytes: &[u8]) -> Option<Head> {
    if &bytes[..8] != MAGIC || !valid(bytes) {
        return None;
    }
    Some(Head {
        sequence: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        binding: Binding {
            change_counter: i64::from_le_bytes(bytes[16..24].try_into().ok()?),
            salt: bytes[24..40].to_vec(),
            index_key: bytes[40..72].to_vec(),
        },
    })
}

fn head(bytes: &[u8]) -> Result<Head> {
    bytes[..2 * HEAD]
        .chunks_exact(HEAD)
        .filter_map(decode_head)
        .max_by_key(|head| head.sequence)
        .context("No readable query history head")
}

fn slot(sequence: u64) -> usize {
    2 * HEAD + ((sequence - 1) % SLOTS as u64) as usize * FRAME
}

fn encode(sequence: u64, item: &QueryObservation) -> [u8; FRAME] {
    let mut bytes = [0; FRAME];
    bytes[..8].copy_from_slice(&sequence.to_le_bytes());
    bytes[8..16].copy_from_slice(&item.observed_at_unix_seconds.to_le_bytes());
    bytes[16..24].copy_from_slice(&item.duration_micros.to_le_bytes());
    bytes[24..32].copy_from_slice(&item.retrieval_micros.unwrap_or(-1).to_le_bytes());
    bytes[32..40].copy_from_slice(&item.rerank_micros.unwrap_or(-1).to_le_bytes());
    bytes[40] = match item.status {
        QueryStatus::Evidence => 1,
        QueryStatus::WeakMatch => 2,
        QueryStatus::Abstained => 3,
        QueryStatus::Error => 4,
    };
    bytes[41] = match item.mode {
        QueryMode::Lexical => 1,
        QueryMode::Vector => 2,
        QueryMode::Hybrid => 3,
    };
    bytes[42] = u8::from(item.graph_requested);
    if let Some(graph) = &item.graph {
        bytes[43] = match graph.seed {
            GraphSeed::Unavailable => 1,
            GraphSeed::NoSeed => 2,
            GraphSeed::Seeded => 3,
            GraphSeed::ResultLimit => 4,
        };
        bytes[44] = u8::from(graph.index_pending)
            | (u8::from(graph.construction_budget_exhausted) << 1)
            | (u8::from(graph.traversal_budget_exhausted) << 2);
    }
    bytes[45] = match item.reranking {
        None => 0,
        Some(RerankStatus::Applied) => 1,
        Some(RerankStatus::Fallback) => 2,
        Some(RerankStatus::Skipped) => 3,
        Some(RerankStatus::PreservedIdentifierOrder) => 4,
    };
    let sum = checksum(&bytes[..48]);
    bytes[48..].copy_from_slice(&sum);
    bytes
}

fn decode(sequence: u64, bytes: &[u8]) -> Option<QueryObservation> {
    if bytes[..8] != sequence.to_le_bytes() || !valid(bytes) {
        return None;
    }
    let status = match bytes[40] {
        1 => QueryStatus::Evidence,
        2 => QueryStatus::WeakMatch,
        3 => QueryStatus::Abstained,
        4 => QueryStatus::Error,
        _ => return None,
    };
    let mode = match bytes[41] {
        1 => QueryMode::Lexical,
        2 => QueryMode::Vector,
        3 => QueryMode::Hybrid,
        _ => return None,
    };
    let seed = match bytes[43] {
        0 => None,
        1 => Some(GraphSeed::Unavailable),
        2 => Some(GraphSeed::NoSeed),
        3 => Some(GraphSeed::Seeded),
        4 => Some(GraphSeed::ResultLimit),
        _ => return None,
    };
    let reranking = match bytes[45] {
        0 => None,
        1 => Some(RerankStatus::Applied),
        2 => Some(RerankStatus::Fallback),
        3 => Some(RerankStatus::Skipped),
        4 => Some(RerankStatus::PreservedIdentifierOrder),
        _ => return None,
    };
    let retrieval = i64::from_le_bytes(bytes[24..32].try_into().ok()?);
    let rerank = i64::from_le_bytes(bytes[32..40].try_into().ok()?);
    Some(QueryObservation {
        observed_at_unix_seconds: i64::from_le_bytes(bytes[8..16].try_into().ok()?),
        status,
        mode,
        graph_requested: bytes[42] == 1,
        graph: seed.map(|seed| GraphObservation {
            seed,
            index_pending: bytes[44] & 1 != 0,
            construction_budget_exhausted: bytes[44] & 2 != 0,
            traversal_budget_exhausted: bytes[44] & 4 != 0,
        }),
        reranking,
        duration_micros: i64::from_le_bytes(bytes[16..24].try_into().ok()?),
        retrieval_micros: (retrieval >= 0).then_some(retrieval),
        rerank_micros: (rerank >= 0).then_some(rerank),
    })
}

fn same_generation(left: &Binding, right: &Binding) -> bool {
    left.salt == right.salt && left.index_key == right.index_key
}

pub(super) fn stored_bytes(state: &Path) -> Result<u64> {
    let len = regular(&state.join(NAME))?.map_or(0, |info| info.len());
    ensure!(len <= RESERVED_BYTES);
    Ok(len)
}

pub(super) struct Writer(File);

#[cfg(not(windows))]
fn try_writer_lock(file: &File) -> io::Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn try_writer_lock(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::LockFile;

    // The byte immediately beyond EOF is a mutex for writers. Windows range
    // locks are mandatory, so the reader's data range must stay unlocked.
    let locked = unsafe { LockFile(file.as_raw_handle(), SIZE as u32, 0, 1, 0) };
    if locked != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(io::ErrorKind::WouldBlock.into())
    } else {
        Err(error)
    }
}

pub(super) fn busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock)
}

#[cfg(windows)]
impl Drop for Writer {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFile;
        // The handle is still open and this is the range acquired by lock().
        unsafe { UnlockFile(self.0.as_raw_handle(), SIZE as u32, 0, 1, 0) };
    }
}

impl Writer {
    pub(super) fn lock(state: &Path, create: bool) -> Result<Option<Self>> {
        let path = state.join(NAME);
        let opened = open_regular(&path, true, false);
        let file = match opened {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                if !create {
                    return Ok(None);
                }
                open_regular(&path, true, true)?
            }
            Err(error) => return Err(error),
        };
        try_writer_lock(&file)?;
        Ok(Some(Self(file)))
    }

    fn read_head(&mut self) -> Result<Head> {
        ensure!(self.0.metadata()?.len() == RESERVED_BYTES);
        let mut bytes = [0; 2 * HEAD];
        self.0.rewind()?;
        self.0.read_exact(&mut bytes)?;
        head(&bytes)
    }

    pub(super) fn ensure_binding(&mut self, binding: &Binding) -> Result<()> {
        if !self
            .read_head()
            .is_ok_and(|head| same_generation(&head.binding, binding))
        {
            self.reset(binding)?;
        }
        Ok(())
    }

    pub(super) fn rewound(&mut self, state: &Path, binding: &Binding) -> Result<bool> {
        let Ok(head) = self.read_head() else {
            return Ok(false);
        };
        Ok(same_generation(&head.binding, binding) && current(&head.binding, state)?.is_none())
    }

    pub(super) fn clear(&mut self, binding: Option<&Binding>) -> Result<()> {
        match binding {
            Some(binding) => self.reset(binding),
            None => {
                let head = self.read_head()?;
                self.reset(&head.binding)
            }
        }
    }

    fn reset(&mut self, binding: &Binding) -> Result<()> {
        self.0.set_len(0)?;
        self.0.set_len(RESERVED_BYTES)?;
        self.0.rewind()?;
        self.0
            .write_all(&encode_head(0, binding, binding.change_counter))?;
        self.0.sync_all()?;
        Ok(())
    }
}

pub(super) fn append(state: &Path, item: &QueryObservation) -> Result<()> {
    let mut writer = Writer::lock(state, false)?.context("Query history spool missing")?;
    ensure!(opt_in(state)?);
    let head = writer.read_head()?;
    let counter = current(&head.binding, state)?.context("The index has changed")?;
    let sequence = head
        .sequence
        .checked_add(1)
        .context("Query history sequence exhausted")?;
    writer.0.seek(SeekFrom::Start(slot(sequence) as u64))?;
    writer.0.write_all(&encode(sequence, item))?;
    writer.0.seek(SeekFrom::Start(sequence % 2 * HEAD as u64))?;
    writer
        .0
        .write_all(&encode_head(sequence, &head.binding, counter))?;
    // This is deliberately not a durability boundary. A lost query event
    // cannot affect its CLI result, durable ingest history or the evidence.
    Ok(())
}

pub(super) fn snapshot(
    state: &Path,
    binding: &Binding,
    enabled: bool,
) -> Result<Option<Vec<QueryObservation>>> {
    let path = state.join(NAME);
    if regular(&path)?.is_none() && !enabled {
        return Ok(Some(Vec::new()));
    }
    let mut file = open_regular(&path, false, false)?;
    ensure!(file.metadata()?.len() == RESERVED_BYTES);
    let mut bytes = vec![0; SIZE];
    file.read_exact(&mut bytes)?;
    let head = head(&bytes)?;
    if !same_generation(binding, &head.binding) || current(&head.binding, state)?.is_none() {
        return Ok(None);
    }
    let first = head
        .sequence
        .saturating_sub(MAX_ROWS as u64)
        .saturating_add(1);
    Ok(Some(
        (first..=head.sequence)
            .filter_map(|sequence| {
                let start = slot(sequence);
                decode(sequence, &bytes[start..start + FRAME])
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::private_state_dir;
    use super::super::{Action, SIDECAR, binding, control, now, summary};
    use super::*;
    use crate::store;
    use std::fs;

    fn observation(duration_micros: i64) -> QueryObservation {
        QueryObservation {
            observed_at_unix_seconds: now(),
            status: QueryStatus::Evidence,
            mode: QueryMode::Hybrid,
            graph_requested: true,
            graph: Some(GraphObservation {
                seed: GraphSeed::Seeded,
                index_pending: false,
                construction_budget_exhausted: false,
                traversal_budget_exhausted: true,
            }),
            reranking: Some(RerankStatus::Fallback),
            duration_micros,
            retrieval_micros: Some(0),
            rerank_micros: None,
        }
    }

    #[test]
    fn full_ring_survives_a_torn_append_and_a_locked_writer() -> Result<()> {
        let state = private_state_dir()?;
        let _index = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        for i in 0..=MAX_ROWS {
            append(state.path(), &observation(i))?;
        }
        let db = rusqlite::Connection::open(state.path().join(SIDECAR))?;
        let bound = binding(&db)?;
        drop(db);
        let mut writer = Writer::lock(state.path(), false)?.unwrap();
        assert!(Writer::lock(state.path(), false).is_err());

        let current = writer.read_head()?;
        let next = current.sequence + 1;
        writer.0.seek(SeekFrom::Start(slot(next) as u64))?;
        writer
            .0
            .write_all(&encode(next, &observation(MAX_ROWS + 1))[..FRAME / 2])?;
        writer.0.seek(SeekFrom::Start(next % 2 * HEAD as u64))?;
        writer.0.write_all(
            &encode_head(next, &current.binding, current.binding.change_counter)[..HEAD / 2],
        )?;

        let retained = snapshot(state.path(), &bound, true)?.unwrap();
        assert_eq!(retained.len(), MAX_ROWS as usize);
        assert_eq!(retained.first().unwrap().duration_micros, 1);
        let latest = retained.last().unwrap();
        assert_eq!(latest.duration_micros, MAX_ROWS);
        assert_eq!(latest.retrieval_micros, Some(0));
        assert!(latest.graph.as_ref().unwrap().traversal_budget_exhausted);
        assert!(matches!(latest.reranking, Some(RerankStatus::Fallback)));

        writer
            .0
            .seek(SeekFrom::Start((slot(current.sequence) + 16) as u64))?;
        writer.0.write_all(&[0xff])?;
        let retained = snapshot(state.path(), &bound, true)?.unwrap();
        assert_eq!(retained.len(), MAX_ROWS as usize - 1);
        assert_eq!(retained.last().unwrap().duration_micros, MAX_ROWS - 1);
        drop(writer);
        let cleared = control(state.path(), Action::Clear)?;
        assert_eq!(cleared.state, "idle");
        assert!(cleared.recent_queries.is_none());
        assert_eq!(stored_bytes(state.path())?, RESERVED_BYTES);
        Ok(())
    }

    #[test]
    fn spool_high_water_detects_index_rewind_without_a_sqlite_history_write() -> Result<()> {
        let state = private_state_dir()?;
        let source = tempfile::tempdir()?;
        let index = state.path().join("personal.sqlite3");
        let mut db = store::open(state.path(), true)?;
        let earlier = fs::read(&index)?;
        control(state.path(), Action::Enable)?;
        fs::write(source.path().join("note.md"), "A synthetic note.")?;
        store::ingest(&mut db, source.path(), true, false, false)?;
        append(state.path(), &observation(1))?;
        assert_eq!(summary(state.path()).recent_queries.unwrap().count, 1);
        drop(db);
        fs::write(&index, earlier)?;
        assert_eq!(
            summary(state.path()).reason,
            Some("index_replaced_or_rolled_back")
        );
        assert_eq!(control(state.path(), Action::Enable)?.state, "idle");
        Ok(())
    }

    #[test]
    fn disable_stops_collection_when_the_spool_is_unusable() -> Result<()> {
        let state = private_state_dir()?;
        let _db = store::open(state.path(), true)?;
        control(state.path(), Action::Enable)?;
        let path = state.path().join(NAME);
        fs::remove_file(&path)?;
        fs::create_dir(&path)?;
        let disabled = control(state.path(), Action::Disable)?;
        assert_eq!(disabled.collection_enabled, Some(false));
        assert!(!opt_in(state.path())?);
        assert!(append(state.path(), &observation(0)).is_err());
        Ok(())
    }
}
