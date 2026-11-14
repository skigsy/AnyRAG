use crate::files::{self, MAX_BATCH_BYTES, MAX_ENTRIES, digest, display_path};
use crate::graph;
use crate::ocr;
use crate::vector;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Write one portable JSON record per indexed passage, without returning corpus text to stdout.
pub fn export(db: &Connection, output: &Path) -> Result<usize> {
    let name = output.file_name().context("Choose a new output file.")?;
    ensure!(
        output
            .extension()
            .is_some_and(|extension| extension == "jsonl"),
        "Choose a .jsonl export path."
    );
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("Choose a new output file in a private directory.")?;
    let parent = files::state_directory(parent, false)?;
    let output = parent.join(name);
    ensure!(
        output
            .symlink_metadata()
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "The export file already exists or is not writable."
    );
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = parent.join(format!(
        ".rust-rag-export-{}-{stamp}.partial",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&temp)?;
    let saved = (|| -> Result<usize> {
        let mut sink = BufWriter::new(file);
        let page = if storage_version(db)? >= ADVANCED_VERSION {
            "c.page"
        } else {
            "NULL"
        };
        let anchor = anchor_sql(db)?;
        let ocr = ocr_sql(db)?;
        let mut statement = db.prepare(&format!(
            "SELECT s.source_id,s.path,s.source_hash,c.chunk_id,c.start,c.end,{page},p.text,{anchor},{ocr}
             FROM sources s JOIN chunks c USING(source_id) JOIN passages p ON p.rowid=c.row_id
             ORDER BY s.source_id,c.start,c.chunk_id"
        ))?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let mut item = json!({
                "schema_version": 1, "id_scheme": "rust-rag-v1",
                "source_id": row.get::<_, String>(0)?, "path": row.get::<_, String>(1)?,
                "source_hash": row.get::<_, String>(2)?, "chunk_id": row.get::<_, String>(3)?,
                "start": row.get::<_, i64>(4)?, "end": row.get::<_, i64>(5)?,
                "page": row.get::<_, Option<i64>>(6)?, "text": row.get::<_, String>(7)?,
                "anchor": row.get::<_, Option<String>>(8)?,
            });
            attach_ocr(&mut item, row, 9)?;
            serde_json::to_writer(&mut sink, &item)?;
            sink.write_all(b"\n")?;
            count += 1;
        }
        sink.flush()?;
        sink.get_ref().sync_all()?;
        drop(sink);
        fs::hard_link(&temp, &output)?;
        Ok(count)
    })();
    let cleanup = fs::remove_file(&temp);
    let count = saved?;
    cleanup.context("Export written; remove the temporary copy in the export directory.")?;
    Ok(count)
}

const APPLICATION_ID: i32 = 0x5241_4450;
const PREVIEW_VERSION: i32 = 1;
const ADVANCED_VERSION: i32 = 2;
const CURRENT_ADVANCED_VERSION: i32 = 3;
const OCR_VERSION: i32 = 4;
const OCR_SYNC_TIMEOUT: Duration = Duration::from_secs(90);
const SQLITE_MIN: i32 = 3_054_001;
const MAX_CHUNKS: i64 = 100_000;
const MAX_QUERY_TERMS: usize = 64;
const STOP_WORDS: &str = "a an and are as at be been being but by can could did do does for from had has have how i if in is it its me my not of on or our please should that the their them then there these they this to was we were what when where which who why will with would you your";

const SCHEMA: &str = "
CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id=1), root TEXT);
INSERT INTO settings(id) VALUES (1);
CREATE TABLE sources (
    source_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, source_hash TEXT NOT NULL
);
CREATE TABLE chunks (
    row_id INTEGER PRIMARY KEY, chunk_id TEXT NOT NULL UNIQUE,
    source_id TEXT NOT NULL REFERENCES sources ON DELETE CASCADE,
    start INTEGER NOT NULL, end INTEGER NOT NULL
);
CREATE INDEX chunks_by_source ON chunks(source_id);
CREATE VIRTUAL TABLE passages USING fts5(text, tokenize='porter unicode61');
INSERT INTO passages(passages,rank) VALUES('secure-delete',1);
";

pub fn open(state: &Path, create: bool) -> Result<Connection> {
    open_with_ocr(state, create, None)
}

pub(crate) fn open_with_ocr(
    state: &Path,
    create: bool,
    fingerprint: Option<&str>,
) -> Result<Connection> {
    open_index(state, create, fingerprint, false)
}

#[cfg(feature = "shared")]
pub(crate) fn open_shared(
    state: &Path,
    create: bool,
    fingerprint: Option<&str>,
) -> Result<Connection> {
    let db = open_index(state, create, fingerprint, true)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    // The shared service checkpoints at sync and retirement boundaries without
    // waiting for readers. Keep automatic checkpoint I/O out of commits.
    db.pragma_update(None, "wal_autocheckpoint", 0)?;
    Ok(db)
}

fn open_index(
    state: &Path,
    create: bool,
    fingerprint: Option<&str>,
    shared: bool,
) -> Result<Connection> {
    ensure!(
        rusqlite::version_number() >= SQLITE_MIN,
        "This build needs SQLite 3.54.1 or newer."
    );
    let path = state.join("personal.sqlite3");
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let fresh = match path.metadata() {
        Ok(info) => info.len() == 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e.into()),
    };
    if create && fresh {
        ensure!(
            [
                "personal.sqlite3-journal",
                "personal.sqlite3-wal",
                "personal.sqlite3-shm"
            ]
            .iter()
            .all(|name| !state.join(name).exists()),
            "State has database sidecars but no active index. Choose a new state folder."
        );
    }
    let mut db = Connection::open_with_flags(path, flags)
        .context("Run init before opening the personal index.")?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA temp_store=MEMORY;")?;
    if create && fresh {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: i32 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if id == 0 {
            let objects: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
            ensure!(objects == 0, "Choose a fresh state folder.");
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            let version = if let Some(fingerprint) = fingerprint {
                ensure!(
                    ocr::valid_fingerprint(fingerprint),
                    "Invalid OCR fingerprint."
                );
                install_advanced(&tx, PREVIEW_VERSION)?;
                tx.execute_batch("ALTER TABLE chunks ADD COLUMN ocr TEXT CHECK (ocr IS NULL OR (page IS NOT NULL AND anchor IS NULL));
                    ALTER TABLE settings ADD COLUMN ocr_fingerprint TEXT;
                    DROP TABLE passages;
                    CREATE VIRTUAL TABLE passages USING fts5(text,table_terms,tokenize='porter unicode61');
                    INSERT INTO passages(passages,rank) VALUES('secure-delete',1);")?;
                tx.execute(
                    "UPDATE settings SET ocr_fingerprint=?1 WHERE id=1",
                    [fingerprint],
                )?;
                OCR_VERSION
            } else {
                PREVIEW_VERSION
            };
            tx.pragma_update(None, "user_version", version)?;
        }
        tx.commit()?;
    }
    let version = compatible_version(&db)?;
    let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    ensure!(
        journal == "delete" || shared && journal == "wal",
        "Expected rollback journal DELETE mode. Restore the index or start fresh."
    );
    let secure_delete: i32 = db.pragma_query_value(None, "secure_delete", |row| row.get(0))?;
    let fts_secure: i32 = db.query_row(
        "SELECT v FROM passages_config WHERE k='secure-delete'",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        secure_delete == 1 && fts_secure == 1,
        "The index needs SQLite and FTS5 secure-delete."
    );
    if fingerprint.is_some() || version == OCR_VERSION {
        let saved = ocr_fingerprint(&db)?;
        ensure!(
            version != OCR_VERSION || saved.as_deref().is_some_and(ocr::valid_fingerprint),
            "Invalid OCR index fingerprint."
        );
        if fingerprint.is_some() {
            ensure!(
                fingerprint == saved.as_deref(),
                "This index needs a matching OCR fingerprint. Use fresh state for a new OCR profile."
            );
        }
    }
    Ok(db)
}

fn compatible_version(db: &Connection) -> Result<i32> {
    let id: i32 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version = storage_version(db)?;
    ensure!(
        supported_version(id, version),
        "This is not a compatible Rust RAG index."
    );
    Ok(version)
}

fn supported_version(id: i32, version: i32) -> bool {
    id == APPLICATION_ID
        && matches!(
            version,
            PREVIEW_VERSION | ADVANCED_VERSION | CURRENT_ADVANCED_VERSION | OCR_VERSION
        )
}

#[cfg(any(feature = "mcp", feature = "dashboard"))]
#[derive(Clone, Copy)]
pub(crate) enum ReadOnlyError {
    Unavailable,
    UnsupportedJournal,
    UnsupportedSchema,
}

#[cfg(feature = "mcp")]
pub(crate) fn open_read_only(state: &Path) -> std::result::Result<Connection, &'static str> {
    open_read_only_with_timeout(state, Duration::from_secs(2)).map_err(|error| match error {
        ReadOnlyError::UnsupportedJournal => "MCP requires a rollback-journal index in DELETE mode. Stop MCP and restore or rebuild the index in DELETE mode.",
        ReadOnlyError::Unavailable | ReadOnlyError::UnsupportedSchema => "Cannot read the personal index. Use an existing compatible index and retry after any writer finishes.",
    })
}

#[cfg(any(feature = "mcp", feature = "dashboard"))]
pub(crate) fn open_read_only_with_timeout(
    state: &Path,
    timeout: Duration,
) -> std::result::Result<Connection, ReadOnlyError> {
    use std::io::Read;

    let path = state.join("personal.sqlite3");
    for name in ["personal.sqlite3-wal", "personal.sqlite3-shm"] {
        match state.join(name).symlink_metadata() {
            Ok(_) => return Err(ReadOnlyError::UnsupportedJournal),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(ReadOnlyError::Unavailable),
        }
    }
    // Opening a WAL database through SQLite can create or update -shm even
    // with SQLITE_OPEN_READ_ONLY. Check the main-file format before SQLite.
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    {
        let mut file = options
            .open(&path)
            .map_err(|_| ReadOnlyError::Unavailable)?;
        let metadata = file.metadata().map_err(|_| ReadOnlyError::Unavailable)?;
        if !metadata.is_file() || files::link_or_unavailable(&metadata) {
            return Err(ReadOnlyError::Unavailable);
        }
        let mut header = [0; 20];
        file.read_exact(&mut header)
            .map_err(|_| ReadOnlyError::Unavailable)?;
        if &header[..16] != b"SQLite format 3\0" {
            return Err(ReadOnlyError::Unavailable);
        }
        if header[18..20] != [1, 1] {
            return Err(ReadOnlyError::UnsupportedJournal);
        }
    }
    let open = || -> Result<Connection> {
        ensure!(rusqlite::version_number() >= SQLITE_MIN);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let db = Connection::open_with_flags(path, flags)?;
        ensure!(db.is_readonly(rusqlite::MAIN_DB)?);
        db.busy_timeout(timeout)?;
        db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        db.execute_batch(
            "PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;",
        )?;
        Ok(db)
    };
    let db = open().map_err(|_| ReadOnlyError::Unavailable)?;
    let mode: String = db
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|_| ReadOnlyError::Unavailable)?;
    if mode != "delete" {
        return Err(ReadOnlyError::UnsupportedJournal);
    }
    let id = db
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|_| ReadOnlyError::Unavailable)?;
    let version = storage_version(&db).map_err(|_| ReadOnlyError::Unavailable)?;
    if !supported_version(id, version) {
        return Err(ReadOnlyError::UnsupportedSchema);
    }
    Ok(db)
}

fn storage_version(db: &Connection) -> Result<i32> {
    Ok(db.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

pub fn require_advanced(db: &Connection) -> Result<()> {
    ensure!(
        storage_version(db)? >= ADVANCED_VERSION,
        "Use a separate index or back up this index, then run init --upgrade."
    );
    Ok(())
}

#[cfg(feature = "dashboard")]
pub(crate) fn page_sql(db: &Connection) -> Result<&'static str> {
    Ok(if storage_version(db)? >= ADVANCED_VERSION {
        "c.page"
    } else {
        "NULL"
    })
}

pub(crate) fn anchor_sql(db: &Connection) -> Result<&'static str> {
    Ok(if storage_version(db)? >= CURRENT_ADVANCED_VERSION {
        "c.anchor"
    } else {
        "NULL"
    })
}

pub(crate) fn ocr_sql(db: &Connection) -> Result<&'static str> {
    Ok(if storage_version(db)? == OCR_VERSION {
        "c.ocr"
    } else {
        "NULL"
    })
}

pub(crate) fn ocr_fingerprint(db: &Connection) -> Result<Option<String>> {
    if storage_version(db)? != OCR_VERSION {
        return Ok(None);
    }
    Ok(db.query_row(
        "SELECT ocr_fingerprint FROM settings WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}

pub(crate) fn attach_ocr(
    item: &mut Value,
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<()> {
    if let Some(stored) = row.get::<_, Option<String>>(index)? {
        item["ocr"] = serde_json::from_str(&stored).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    }
    Ok(())
}

fn install_advanced(db: &Connection, old_version: i32) -> Result<()> {
    if old_version == PREVIEW_VERSION {
        db.execute_batch("ALTER TABLE chunks ADD COLUMN page INTEGER CHECK (page > 0);")?;
        db.execute_batch(graph::SCHEMA)?;
        db.execute_batch(vector::SCHEMA)?;
    }
    db.execute_batch(
        "ALTER TABLE chunks ADD COLUMN anchor TEXT CHECK (anchor IS NULL OR page IS NULL);",
    )?;
    db.execute_batch(crate::enrichment::SCHEMA)?;
    Ok(())
}

#[cfg(feature = "model")]
pub(crate) fn require_enrichment(db: &Connection) -> Result<()> {
    ensure!(
        storage_version(db)? >= CURRENT_ADVANCED_VERSION,
        "Run init --upgrade to enable durable enrichment on this index."
    );
    Ok(())
}

pub fn upgrade(db: &mut Connection, state: &Path) -> Result<Option<PathBuf>> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut backup = None;
    let old_version = storage_version(&tx)?;
    if old_version < CURRENT_ADVANCED_VERSION {
        let has_state: bool = tx.query_row(
            "SELECT root IS NOT NULL FROM settings WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if has_state {
            let journal: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
            ensure!(
                journal == "delete",
                "Convert older indexes to DELETE mode before upgrading."
            );
            // DELETE journaling plus the reserved writer lock makes the unchanged
            // database file a consistent snapshot before the first schema write.
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = state.join(format!(
                "personal-v{old_version}-{timestamp}.sqlite3.backup"
            ));
            let partial = path.with_extension("partial");
            let mut source = File::open(state.join("personal.sqlite3"))?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut destination = options.open(&partial)?;
            let saved =
                io::copy(&mut source, &mut destination).and_then(|_| destination.sync_all());
            drop(destination);
            let published = saved.and_then(|_| fs::hard_link(&partial, &path));
            let cleanup = fs::remove_file(&partial);
            published?;
            cleanup?;
            backup = Some(path);
        }
        install_advanced(&tx, old_version)?;
        tx.pragma_update(None, "user_version", CURRENT_ADVANCED_VERSION)?;
    }
    tx.commit()?;
    Ok(backup)
}

pub fn status(db: &Connection) -> Result<Value> {
    let version = storage_version(db)?;
    let mut item = db.query_row(
        "SELECT root, (SELECT count(*) FROM sources), (SELECT count(*) FROM chunks) FROM settings WHERE id=1",
        [],
        |row| Ok(json!({
            "schema_version": 1, "storage_version": version, "engine": "rust-rag",
            "id_scheme": "rust-rag-v1", "root": row.get::<_, Option<String>>(0)?,
            "sources": row.get::<_, i64>(1)?, "chunks": row.get::<_, i64>(2)?,
            "mode": "lexical", "graph": version >= ADVANCED_VERSION,
            "pdf_reader": cfg!(feature = "pdf"), "model_endpoint": cfg!(feature = "model"),
            "docx_reader": cfg!(feature = "docx") && version >= CURRENT_ADVANCED_VERSION,
        })),
    )?;
    if let Some(fingerprint) = ocr_fingerprint(db)? {
        item["ocr_fingerprint"] = json!(fingerprint);
    }
    Ok(item)
}

fn retire(db: &Connection, id: &str) -> Result<usize> {
    db.execute(
        "DELETE FROM passages WHERE rowid IN (SELECT row_id FROM chunks WHERE source_id=?1)",
        [id],
    )?;
    Ok(db.execute("DELETE FROM sources WHERE source_id=?1", [id])?)
}

struct LocatedChunk {
    start: usize,
    end: usize,
    page: Option<usize>,
    anchor: Option<String>,
    quote: String,
    ocr: Option<(Value, String)>,
}

fn text_chunks(
    text: &str,
    page: Option<usize>,
    offset: usize,
) -> impl Iterator<Item = LocatedChunk> {
    files::chunks(text).map(move |(start, end, quote)| LocatedChunk {
        start: start + offset,
        end: end + offset,
        page,
        anchor: None,
        quote: quote.to_owned(),
        ocr: None,
    })
}

fn parse(raw: &[u8], pdf: bool, docx: bool, xlsx: bool) -> Result<Vec<LocatedChunk>> {
    if xlsx {
        #[cfg(feature = "xlsx")]
        {
            let mut offset = 0;
            let mut chunks = Vec::new();
            for cell in crate::xlsx::extract(raw)? {
                chunks.extend(text_chunks(&cell.text, None, offset).map(|mut chunk| {
                    chunk.anchor = Some(cell.anchor.clone());
                    chunk
                }));
                offset += cell.text.chars().count() + 2;
            }
            return Ok(chunks);
        }
        #[cfg(not(feature = "xlsx"))]
        anyhow::bail!("Build with the xlsx feature to read Excel workbooks.");
    }
    if docx {
        #[cfg(feature = "docx")]
        {
            let mut offset = 0;
            let mut chunks = Vec::new();
            for paragraph in crate::docx::extract(raw)? {
                chunks.extend(text_chunks(&paragraph.text, None, offset).map(|mut chunk| {
                    chunk.anchor = Some(paragraph.anchor.clone());
                    chunk
                }));
                offset += paragraph.text.chars().count() + 2;
            }
            return Ok(chunks);
        }
        #[cfg(not(feature = "docx"))]
        anyhow::bail!("Build with the docx feature to read Word files.");
    }
    if !pdf {
        return Ok(text_chunks(&files::decode(raw)?, None, 0).collect());
    }
    #[cfg(feature = "pdf")]
    {
        let mut offset = 0;
        let mut chunks = Vec::new();
        for (page, text) in crate::pdf::extract(raw)?.iter().enumerate() {
            chunks.extend(text_chunks(text, Some(page + 1), offset));
            offset += text.chars().count() + 2;
        }
        Ok(chunks)
    }
    #[cfg(not(feature = "pdf"))]
    anyhow::bail!("Build with the pdf feature to read PDF files.")
}

struct IngestState {
    advanced: bool,
    anchors: bool,
    ocr: bool,
    chunks_total: i64,
}

impl IngestState {
    fn new(db: &Connection) -> Result<Self> {
        let version = storage_version(db)?;
        Ok(Self {
            advanced: version >= ADVANCED_VERSION,
            anchors: version >= CURRENT_ADVANCED_VERSION,
            ocr: version == OCR_VERSION,
            chunks_total: db.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))?,
        })
    }

    fn publish(
        &mut self,
        db: &Connection,
        id: &str,
        source: &str,
        hash: &str,
        replace: bool,
        chunks: Vec<LocatedChunk>,
    ) -> Result<()> {
        if replace {
            self.chunks_total -= db.query_row(
                "SELECT count(*) FROM chunks WHERE source_id=?1",
                [id],
                |row| row.get::<_, i64>(0),
            )?;
            retire(db, id)?;
        }
        self.chunks_total += chunks.len() as i64;
        ensure!(
            self.chunks_total <= MAX_CHUNKS,
            "The index exceeds the {MAX_CHUNKS}-passage limit."
        );
        db.execute(
            "INSERT INTO sources VALUES (?1,?2,?3)",
            params![id, source, hash],
        )?;
        let mut insert = db.prepare(if self.ocr {
            "INSERT INTO chunks(chunk_id,source_id,start,end,page,anchor,ocr) VALUES (?1,?2,?3,?4,?5,?6,?7)"
        } else if self.anchors {
            "INSERT INTO chunks(chunk_id,source_id,start,end,page,anchor) VALUES (?1,?2,?3,?4,?5,?6)"
        } else if self.advanced {
            "INSERT INTO chunks(chunk_id,source_id,start,end,page) VALUES (?1,?2,?3,?4,?5)"
        } else {
            "INSERT INTO chunks(chunk_id,source_id,start,end) VALUES (?1,?2,?3,?4)"
        })?;
        let mut insert_fts = db.prepare(if self.ocr {
            "INSERT INTO passages(rowid,text,table_terms) VALUES (?1,?2,?3)"
        } else {
            "INSERT INTO passages(rowid,text) VALUES (?1,?2)"
        })?;
        for LocatedChunk {
            start,
            end,
            page,
            anchor,
            quote,
            ocr,
        } in chunks
        {
            let identity = if let Some((provenance, _)) = &ocr {
                let rendition = provenance["rendition_id"]
                    .as_str()
                    .context("Missing OCR rendition identity.")?;
                format!("{id}:{rendition}:pdf-ocr-v1:{start}:{end}")
            } else {
                match (page, anchor.as_deref()) {
                    (Some(page), _) => format!("{id}:{hash}:pdf-reader-v1:{page}:{start}:{end}"),
                    (_, Some(anchor)) if anchor.starts_with("sheet:") => {
                        format!("{id}:{hash}:xlsx-reader-v1:{anchor}:{start}:{end}")
                    }
                    (_, Some(anchor)) => {
                        format!("{id}:{hash}:docx-reader-v1:{anchor}:{start}:{end}")
                    }
                    _ => format!("{id}:{hash}:{start}:{end}"),
                }
            };
            let chunk_id = digest(identity.as_bytes());
            if self.ocr {
                let (provenance, terms) = match ocr {
                    Some((provenance, terms)) => (Some(serde_json::to_string(&provenance)?), terms),
                    None => (None, String::new()),
                };
                insert.execute(params![
                    chunk_id,
                    id,
                    start as i64,
                    end as i64,
                    page.map(|page| page as i64),
                    anchor,
                    provenance
                ])?;
                insert_fts.execute(params![db.last_insert_rowid(), quote, terms])?;
                continue;
            } else if self.anchors {
                insert.execute(params![
                    chunk_id,
                    id,
                    start as i64,
                    end as i64,
                    page.map(|page| page as i64),
                    anchor,
                ])?;
            } else if self.advanced {
                insert.execute(params![
                    chunk_id,
                    id,
                    start as i64,
                    end as i64,
                    page.map(|page| page as i64)
                ])?;
            } else {
                insert.execute(params![chunk_id, id, start as i64, end as i64])?;
            }
            insert_fts.execute(params![db.last_insert_rowid(), quote])?;
        }
        Ok(())
    }
}

fn require_root(db: &Connection, root: &str) -> Result<()> {
    let current: Option<String> =
        db.query_row("SELECT root FROM settings WHERE id=1", [], |row| row.get(0))?;
    ensure!(
        current.as_deref().is_none_or(|current| current == root),
        "This index already tracks another source. Use a separate --state-dir."
    );
    Ok(())
}

#[cfg(feature = "aws")]
fn retire_missing(db: &Connection, present: &HashSet<String>) -> Result<usize> {
    let mut sources = db.prepare("SELECT source_id FROM sources")?;
    let missing = sources
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut removed = 0;
    for id in missing.iter().filter(|id| !present.contains(*id)) {
        removed += retire(db, id)?;
    }
    Ok(removed)
}

const LOCAL_PROGRESS: &str = "
CREATE TABLE IF NOT EXISTS local_ingest_progress (
    ordinal INTEGER PRIMARY KEY,
    source_id TEXT NOT NULL UNIQUE,
    stamp TEXT NOT NULL,
    source_hash TEXT
);
";

#[derive(Default)]
struct LocalReport {
    updated: usize,
    unchanged: usize,
    empty: usize,
    deferred: usize,
    ocr_deferred: usize,
    priority_deferred: usize,
    removed: usize,
    skipped_pdfs: usize,
    skipped_docx: usize,
    skipped_xlsx: usize,
    failures: Vec<Value>,
    sync_reason: Option<String>,
    source_review: Option<Vec<Value>>,
}

impl LocalReport {
    fn source(&mut self, path: &str, status: &str, reason: Option<&str>) {
        if let Some(review) = &mut self.source_review {
            review.push(json!({"path": path, "status": status, "reason": reason}));
        }
    }

    fn review_deferred(
        &mut self,
        db: &Connection,
        work: &ContentWork<'_>,
        reason: &str,
    ) -> Result<()> {
        if let Some(review) = &mut self.source_review {
            let retained = source_hash(db, &work.id)?.is_some();
            let (status, reason) = if !work.priority && retained {
                (
                    "recheck_deferred",
                    "Indexed revision kept. A content recheck will run in a later ingest.",
                )
            } else {
                ("deferred", reason)
            };
            review.push(json!({
                "path": work.source, "status": status, "reason": reason, "retained_previous": retained,
            }));
        }
        Ok(())
    }

    fn failure(&mut self, source: &str, stage: &str, reason: &str, retained: bool) {
        let failure = json!({
            "path": source, "stage": stage, "reason": reason, "retained_previous": retained,
        });
        if let Some(review) = &mut self.source_review
            && stage != "inventory"
        {
            let mut row = failure.clone();
            row["status"] = json!("failed");
            review.push(row);
        }
        self.failures.push(failure);
    }

    fn json(self, root: &str, inventory: Option<&files::Inventory>, bytes_read: usize) -> Value {
        let mut report = json!({
            "schema_version": 1, "updated_sources": self.updated, "unchanged_sources": self.unchanged,
            "empty_sources": self.empty, "deferred_sources": self.deferred,
            "priority_deferred_sources": self.priority_deferred,
            "removed_sources": self.removed, "failures": self.failures,
            "sync_blocked": self.sync_reason.is_some(), "sync_reason": self.sync_reason,
            "inventory_complete": inventory.is_some_and(|item| item.skipped_links == 0),
            "skipped_links_or_placeholders": inventory.map(|item| item.skipped_links),
            "scanned_entries": inventory.map(|item| item.entries),
            "skipped_pdfs": self.skipped_pdfs, "skipped_docx": self.skipped_docx, "skipped_xlsx": self.skipped_xlsx,
            "bytes_read": bytes_read, "max_scan_entries": MAX_ENTRIES,
            "byte_budget_exhausted": self.deferred > self.ocr_deferred,
        });
        if self.ocr_deferred > 0 {
            report["ocr_deferred_sources"] = json!(self.ocr_deferred);
        }
        if let Some(mut review) = self.source_review {
            for row in &mut review {
                if let Some(relative) = row["path"]
                    .as_str()
                    .and_then(|path| Path::new(path).strip_prefix(root).ok())
                    .and_then(Path::to_str)
                {
                    row["path"] = json!(relative);
                }
            }
            review.sort_unstable_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            report["source_review"] = json!(review);
        }
        report
    }
}

struct ContentWork<'a> {
    path: &'a Path,
    source: &'a str,
    id: String,
    stamp: files::FileStamp,
    hint: String,
    ordinal: i64,
    priority: bool,
}

type OcrExtraction<'a> =
    dyn FnMut(&ocr::Worker, &[u8], &str, Instant) -> Result<Result<Vec<ocr::Passage>>> + 'a;

pub(crate) struct OcrBatch<'a> {
    worker: Option<&'a ocr::Worker>,
    deadline: Option<Instant>,
    extraction: Option<&'a mut OcrExtraction<'a>>,
    publish_gate: Option<&'a std::sync::RwLock<()>>,
}

impl<'a> OcrBatch<'a> {
    pub fn new(worker: Option<&'a ocr::Worker>) -> Self {
        Self {
            worker,
            deadline: None,
            extraction: None,
            publish_gate: None,
        }
    }

    #[cfg(feature = "shared")]
    pub(crate) fn with_extraction(mut self, extract: &'a mut OcrExtraction<'a>) -> Self {
        self.extraction = Some(extract);
        self
    }

    #[cfg(feature = "shared")]
    pub(crate) fn with_publish_gate(mut self, gate: &'a std::sync::RwLock<()>) -> Self {
        self.publish_gate = Some(gate);
        self
    }
}

fn publish_local(
    tx: rusqlite::Transaction<'_>,
    gate: Option<&std::sync::RwLock<()>>,
) -> Result<()> {
    let _visibility = gate
        .map(|gate| {
            gate.write()
                .map_err(|_| anyhow::anyhow!("Index visibility is unavailable."))
        })
        .transpose()?;
    tx.commit()?;
    Ok(())
}

fn source_hash(db: &Connection, id: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT source_hash FROM sources WHERE source_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

fn record_attempt(
    db: &Connection,
    work: &ContentWork<'_>,
    hint: &str,
    current: Option<&str>,
) -> Result<()> {
    db.execute("INSERT OR REPLACE INTO local_ingest_progress(source_id,stamp,source_hash) VALUES(?1,?2,?3)",
               params![work.id, hint, current])?;
    Ok(())
}

fn process_local(
    db: &Connection,
    state: &mut IngestState,
    work: ContentWork<'_>,
    remaining: &mut usize,
    report: &mut LocalReport,
    ocr: &mut OcrBatch<'_>,
) -> Result<()> {
    // Reserve a complete second read and one extra byte per read to detect
    // growth. Deferred items keep their durable priority.
    if 2 * (work.stamp.bytes as usize + 1) > *remaining {
        report.deferred += 1;
        report.priority_deferred += usize::from(work.priority);
        report.review_deferred(
            db,
            &work,
            "The scan reached its byte limit. Run ingest again.",
        )?;
        return Ok(());
    }
    let old_hash = source_hash(db, &work.id)?;
    let raw = files::read_revision(work.path, remaining, &work.stamp);
    let mut stage = "read";
    let pdf = files::is_pdf(work.path);
    let prepared = match raw {
        Err(error) => Err(error),
        Ok(raw) => {
            let hash = digest(&raw);
            if old_hash.as_deref() == Some(&hash) {
                record_attempt(db, &work, &work.hint, Some(&hash))?;
                report.unchanged += 1;
                report.source(
                    work.source,
                    "unchanged",
                    Some("The source bytes match the indexed revision."),
                );
                return Ok(());
            }
            stage = "parse";
            let chunks = if let Some(worker) = ocr.worker.filter(|_| pdf) {
                let deadline = *ocr
                    .deadline
                    .get_or_insert_with(|| Instant::now() + OCR_SYNC_TIMEOUT);
                if deadline.saturating_duration_since(Instant::now()) < ocr::JOB_TIMEOUT {
                    report.deferred += 1;
                    report.ocr_deferred += 1;
                    report.priority_deferred += usize::from(work.priority);
                    report.review_deferred(
                        db,
                        &work,
                        "The OCR batch reached its time limit. Run ingest again.",
                    )?;
                    return Ok(());
                }
                let extracted = match &mut ocr.extraction {
                    // An outer error interrupts the sync before publication. A
                    // worker failure only retains this document's old revision.
                    Some(extract) => extract(worker, &raw, &hash, deadline)?,
                    None => worker.extract(&raw, &hash, deadline),
                };
                extracted.map(|passages| {
                    passages
                        .into_iter()
                        .map(|passage| LocatedChunk {
                            start: passage.start,
                            end: passage.end,
                            page: Some(passage.page),
                            anchor: None,
                            quote: passage.text,
                            ocr: Some((passage.provenance, passage.table_terms)),
                        })
                        .collect()
                })
            } else {
                parse(
                    &raw,
                    pdf,
                    files::is_docx(work.path),
                    files::is_xlsx(work.path),
                )
            };
            chunks.and_then(|chunks| {
                ensure!(!pdf || !chunks.is_empty(), "No readable text.");
                stage = "revalidate";
                let confirmation = files::read_revision(work.path, remaining, &work.stamp)?;
                ensure!(
                    confirmation == raw,
                    "The source changed while it was being parsed. Run ingest again."
                );
                Ok((hash, chunks))
            })
        }
    };
    match prepared {
        Ok((hash, chunks)) => {
            let empty = chunks.is_empty();
            state.publish(db, &work.id, work.source, &hash, old_hash.is_some(), chunks)?;
            record_attempt(db, &work, &work.hint, Some(&hash))?;
            report.updated += 1;
            report.empty += usize::from(empty);
            if empty {
                report.source(
                    work.source,
                    "empty",
                    Some("No indexable text. Any previous passages were retired."),
                );
            } else {
                report.source(work.source, "indexed", None);
            }
        }
        Err(error) => {
            // Advance past unreadable work as well, so it cannot starve the queue.
            record_attempt(db, &work, "", old_hash.as_deref())?;
            report.failure(work.source, stage, &error.to_string(), old_hash.is_some());
        }
    }
    Ok(())
}

fn retire_local_missing(
    db: &mut Connection,
    root: &Path,
    root_text: &str,
    inventory: &files::Inventory,
    stamp: &files::FileStamp,
    report: &mut LocalReport,
    gate: Option<&std::sync::RwLock<()>>,
) -> Result<(usize, Option<String>)> {
    if inventory.skipped_links != 0 {
        return Ok((
            0,
            Some("The fresh inventory contains a link or unavailable entry.".into()),
        ));
    }
    let present = inventory
        .files
        .iter()
        .map(|path| display_path(path).map(|name| digest(name.as_bytes())))
        .collect::<Result<HashSet<_>>>()?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_root(&tx, root_text)?;
    let missing = {
        let mut query = tx.prepare("SELECT source_id,path FROM sources")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(id, _)| !present.contains(id))
            .collect::<Vec<_>>()
    };
    let mut removed = 0;
    for (id, path) in &missing {
        match files::still_absent(root, path) {
            Ok(true) => {
                removed += retire(&tx, id)?;
                tx.execute("DELETE FROM local_ingest_progress WHERE source_id=?1", [id])?;
            }
            Ok(false) => {
                return Ok((
                    0,
                    Some("An indexed path reappeared after the fresh inventory.".into()),
                ));
            }
            Err(error) => return Ok((0, Some(error.to_string()))),
        }
    }
    // Recheck once more inside the retirement transaction. A race cannot
    // authorize a partial prune when a formerly absent path has reappeared.
    for (_, path) in &missing {
        if !matches!(files::still_absent(root, path), Ok(true)) {
            return Ok((
                0,
                Some("An indexed path changed during removal confirmation.".into()),
            ));
        }
    }
    if !matches!(files::folder_stamp(root), Ok(ref current) if current == stamp) {
        return Ok((
            0,
            Some("The source root changed during removal confirmation.".into()),
        ));
    }
    publish_local(tx, gate)?;
    if report.source_review.is_some() {
        for (_, path) in &missing {
            report.source(
                path,
                "removed",
                Some("The source was removed from the index after sync."),
            );
        }
    }
    Ok((removed, None))
}

fn reconcile_local(
    db: &mut Connection,
    root: &Path,
    root_text: &str,
    report: &mut LocalReport,
    gate: Option<&std::sync::RwLock<()>>,
) -> Result<(usize, Option<String>)> {
    let stamp = files::folder_stamp(root);
    let snapshot =
        stamp.and_then(|stamp| files::inventory(root).map(|inventory| (stamp, inventory)));
    match snapshot {
        Ok((stamp, inventory)) => {
            retire_local_missing(db, root, root_text, &inventory, &stamp, report, gate)
        }
        Err(error) => Ok((0, Some(error.to_string()))),
    }
}

pub fn ingest(
    db: &mut Connection,
    root: &Path,
    sync: bool,
    include_pdf: bool,
    include_docx: bool,
) -> Result<Value> {
    ingest_with_ocr(
        db,
        root,
        sync,
        include_pdf,
        include_docx,
        &mut OcrBatch::new(None),
    )
}

pub(crate) fn ingest_with_ocr(
    db: &mut Connection,
    root: &Path,
    sync: bool,
    include_pdf: bool,
    include_docx: bool,
    ocr: &mut OcrBatch<'_>,
) -> Result<Value> {
    ingest_with_options(
        db,
        root,
        LocalIngestOptions {
            sync,
            pdf: include_pdf,
            docx: include_docx,
            xlsx: false,
            review: false,
        },
        ocr,
    )
}

#[derive(Default)]
pub(crate) struct LocalIngestOptions {
    pub sync: bool,
    pub pdf: bool,
    pub docx: bool,
    pub xlsx: bool,
    pub review: bool,
}

pub(crate) fn ingest_with_options(
    db: &mut Connection,
    root: &Path,
    options: LocalIngestOptions,
    ocr: &mut OcrBatch<'_>,
) -> Result<Value> {
    ingest_local(db, root, options, MAX_BATCH_BYTES, ocr)
}

fn ingest_local(
    db: &mut Connection,
    root: &Path,
    options: LocalIngestOptions,
    byte_budget: usize,
    ocr: &mut OcrBatch<'_>,
) -> Result<Value> {
    ensure!(
        ocr_fingerprint(db)?.as_deref() == ocr.worker.map(ocr::Worker::fingerprint),
        "Ingest requires the matching OCR profile for this index. Use fresh state for a new OCR profile."
    );
    if options.pdf {
        ensure!(
            cfg!(feature = "pdf"),
            "Build with the pdf feature to read PDF files."
        );
        require_advanced(db)?;
    }
    if options.docx {
        ensure!(
            cfg!(feature = "docx"),
            "Build with the docx feature to read DOCX files."
        );
        ensure!(
            IngestState::new(db)?.anchors,
            "Run init --upgrade before indexing DOCX files."
        );
    }
    if options.xlsx {
        ensure!(
            cfg!(feature = "xlsx"),
            "Build with the xlsx feature to read Excel workbooks."
        );
        ensure!(
            IngestState::new(db)?.anchors,
            "Run init --upgrade before indexing Excel workbooks."
        );
    }
    let root_text = display_path(root)?;
    require_root(db, root_text)?;
    let mut report = LocalReport {
        source_review: options.review.then(Vec::new),
        ..LocalReport::default()
    };
    let inventory = match files::inventory_with_review(root, options.review) {
        Ok(inventory) => inventory,
        Err(error) => {
            let reason = error.to_string();
            report.failure(root_text, "inventory", &reason, false);
            report.sync_reason = options.sync.then_some(reason);
            return Ok(report.json(root_text, None, 0));
        }
    };
    for (path, reason) in &inventory.skipped {
        report.source(
            display_path(path).unwrap_or(&path.to_string_lossy()),
            "skipped",
            Some(reason),
        );
    }
    let mut present = HashSet::new();
    let mut urgent = Vec::new();
    let mut periodic = Vec::new();
    {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_root(&tx, root_text)?;
        tx.execute_batch(LOCAL_PROGRESS)?;
        tx.execute("UPDATE settings SET root=?1 WHERE id=1", [root_text])?;
        let mut progress = tx.prepare(
            "SELECT stamp, ordinal, source_hash FROM local_ingest_progress WHERE source_id=?1",
        )?;
        for path in &inventory.files {
            let source = display_path(path)?;
            let id = digest(source.as_bytes());
            present.insert(id.clone());
            if files::is_pdf(path) && !options.pdf && ocr.worker.is_none() {
                report.skipped_pdfs += 1;
                report.source(
                    source,
                    "skipped",
                    Some("PDF reading is off. Use an advanced index and --pdf on a build with the pdf feature for text-layer PDFs."),
                );
                continue;
            }
            if files::is_docx(path) && !options.docx {
                report.skipped_docx += 1;
                report.source(
                    source,
                    "skipped",
                    Some("DOCX reading is off. Use an advanced index and --docx on a build with the docx feature."),
                );
                continue;
            }
            if files::is_xlsx(path) && !options.xlsx {
                report.skipped_xlsx += 1;
                report.source(source, "skipped", Some("Excel reading is off. Use an advanced index and --xlsx on a build with the xlsx feature."));
                continue;
            }
            let old_hash = source_hash(&tx, &id)?;
            let stamp = match files::file_stamp(path) {
                Ok(stamp) if stamp.bytes <= files::MAX_BYTES as u64 => stamp,
                Ok(_) => {
                    report.failure(
                        source,
                        "read",
                        "The file exceeds the 16 MiB limit.",
                        old_hash.is_some(),
                    );
                    continue;
                }
                Err(error) => {
                    report.failure(source, "read", &error.to_string(), old_hash.is_some());
                    continue;
                }
            };
            let hint = stamp.hint();
            let previous: Option<(String, i64, Option<String>)> = progress
                .query_row([&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .optional()?;
            let priority = previous
                .as_ref()
                .is_none_or(|(stamp, _, hash)| *stamp != hint || *hash != old_hash);
            let work = ContentWork {
                path,
                source,
                id,
                stamp,
                hint,
                ordinal: previous.as_ref().map_or(0, |row| row.1),
                priority,
            };
            if priority {
                urgent.push(work);
            } else {
                periodic.push(work);
            }
        }
        drop(progress);
        let prior_ids = {
            let mut query = tx.prepare("SELECT source_id FROM local_ingest_progress")?;
            query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in prior_ids.iter().filter(|id| !present.contains(*id)) {
            tx.execute("DELETE FROM local_ingest_progress WHERE source_id=?1", [id])?;
        }
        publish_local(tx, ocr.publish_gate)?;
    }
    urgent.sort_unstable_by(|a, b| (a.ordinal, a.source).cmp(&(b.ordinal, b.source)));
    periodic.sort_unstable_by(|a, b| (a.ordinal, a.source).cmp(&(b.ordinal, b.source)));
    let mut urgent = urgent.into_iter();
    let mut periodic = periodic.into_iter();
    // Give the oldest routine rehash a turn even with a continuous stream of
    // metadata changes. The production budget can fit both first items.
    let mut work = urgent
        .next()
        .into_iter()
        .chain(periodic.next())
        .chain(urgent)
        .chain(periodic)
        .peekable();
    let small_text = |item: &ContentWork<'_>| {
        item.stamp.bytes <= 64 * 1024
            && !files::is_pdf(item.path)
            && !files::is_docx(item.path)
            && !files::is_xlsx(item.path)
    };
    let mut remaining = byte_budget;
    while let Some(item) = work.next() {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_root(&tx, root_text)?;
        let mut state = IngestState::new(&tx)?;
        let batch = small_text(&item);
        process_local(&tx, &mut state, item, &mut remaining, &mut report, ocr)?;
        if batch {
            // Checkpoint up to 1 MiB of small text files together to amortize
            // journal flushes. A larger file or parser runs in its own batch.
            for _ in 1..16 {
                if !work.peek().is_some_and(&small_text) {
                    break;
                }
                let item = work.next().expect("peeked ingestion item");
                process_local(&tx, &mut state, item, &mut remaining, &mut report, ocr)?;
            }
        }
        publish_local(tx, ocr.publish_gate)?;
    }
    if options.sync {
        (report.removed, report.sync_reason) =
            reconcile_local(db, root, root_text, &mut report, ocr.publish_gate)?;
    }
    Ok(report.json(root_text, Some(&inventory), byte_budget - remaining))
}

#[cfg(feature = "shared")]
pub(crate) fn reset_local_ingest_progress(db: &Connection) -> Result<()> {
    db.execute_batch(LOCAL_PROGRESS)?;
    db.execute("DELETE FROM local_ingest_progress", [])?;
    Ok(())
}

#[cfg(feature = "aws")]
pub fn ingest_s3(
    db: &mut Connection,
    source: &crate::cloud::S3Source,
    include_pdf: bool,
) -> Result<Value> {
    if include_pdf {
        ensure!(
            cfg!(feature = "pdf"),
            "Build with the pdf feature to read PDF files."
        );
        require_advanced(db)?;
    }
    const MAX_SOURCE: u64 = 16 * 1024 * 1024;
    let root = source.identity();
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_root(&tx, &root)?;
    let inventory = source.inventory(MAX_ENTRIES, MAX_SOURCE)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS s3_versions (
            source_id TEXT PRIMARY KEY NOT NULL REFERENCES sources ON DELETE CASCADE,
            version TEXT NOT NULL
        );",
    )?;
    let mut state = IngestState::new(&tx)?;
    let mut present = HashSet::new();
    let mut failures = Vec::new();
    let mut updated = 0;
    let mut unchanged = 0;
    let mut skipped_pdfs = 0;
    let mut remaining = MAX_BATCH_BYTES as u64;
    let mut exhausted = false;
    for entry in &inventory.entries {
        let uri = source.uri(entry);
        let id = digest(uri.as_bytes());
        present.insert(id.clone());
        let pdf = entry.suffix == "pdf";
        let docx = entry.suffix == "docx";
        if pdf && !include_pdf {
            skipped_pdfs += 1;
            continue;
        }
        if docx && !cfg!(feature = "docx") {
            continue;
        }
        let old: Option<(String, Option<String>)> = tx.query_row(
            "SELECT s.source_hash,v.version FROM sources s LEFT JOIN s3_versions v USING(source_id)
             WHERE s.source_id=?1", [&id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if old.as_ref().and_then(|(_, version)| version.as_deref()) == Some(entry.revision.as_str())
        {
            unchanged += 1;
            continue;
        }
        if entry.size > remaining {
            exhausted = true;
            failures.push(json!({"path": uri, "reason": "The S3 scan exceeded its byte budget."}));
            continue;
        }
        remaining -= entry.size;
        let raw = match source.read(entry, MAX_SOURCE) {
            Ok(raw) => raw,
            Err(error) => {
                failures.push(json!({"path": uri, "reason": error.to_string()}));
                continue;
            }
        };
        let hash = digest(&raw);
        if old.as_ref().is_none_or(|(old_hash, _)| old_hash != &hash) {
            let chunks = match parse(&raw, pdf, docx, false) {
                Ok(chunks) if !pdf || !chunks.is_empty() => chunks,
                Ok(_) => {
                    failures.push(json!({"path": uri, "reason": "No readable text."}));
                    continue;
                }
                Err(error) => {
                    failures.push(json!({"path": uri, "reason": error.to_string()}));
                    continue;
                }
            };
            state.publish(&tx, &id, &uri, &hash, old.is_some(), chunks)?;
        }
        tx.execute(
            "INSERT INTO s3_versions(source_id,version) VALUES(?1,?2)
             ON CONFLICT(source_id) DO UPDATE SET version=excluded.version",
            params![id, entry.revision],
        )?;
        updated += 1;
    }
    let removed = if exhausted {
        0
    } else {
        let confirmed = source.inventory(MAX_ENTRIES, MAX_SOURCE)?;
        ensure!(
            confirmed.entries == inventory.entries,
            "The S3 inventory changed during synchronization. Retry the complete scan."
        );
        retire_missing(&tx, &present)?
    };
    tx.execute("UPDATE settings SET root=?1 WHERE id=1", [root])?;
    tx.commit()?;
    Ok(json!({
        "schema_version": 1, "updated_sources": updated, "unchanged_sources": unchanged,
        "removed_sources": removed, "failures": failures, "sync_blocked": exhausted,
        "skipped_links_or_placeholders": 0, "scanned_entries": inventory.examined,
        "skipped_pdfs": skipped_pdfs, "max_scan_entries": MAX_ENTRIES, "byte_budget_exhausted": exhausted,
    }))
}

#[cfg(feature = "aws")]
pub fn source_revision_sql(db: &Connection) -> Result<(&'static str, &'static str)> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='s3_versions')",
        [],
        |row| row.get(0),
    )?;
    Ok(if exists {
        (
            "LEFT JOIN s3_versions v ON v.source_id=s.source_id",
            "COALESCE(v.version,s.source_hash)",
        )
    } else {
        ("", "s.source_hash")
    })
}

#[cfg(not(feature = "aws"))]
pub fn source_revision_sql(_: &Connection) -> Result<(&'static str, &'static str)> {
    Ok(("", "s.source_hash"))
}

fn terms(question: &str) -> Vec<String> {
    let normalized = question.to_lowercase();
    let mut seen = HashSet::new();
    normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| {
            !term.is_empty()
                && !STOP_WORDS
                    .split_ascii_whitespace()
                    .any(|word| word == *term)
        })
        .map(|term| term.chars().take(100).collect::<String>())
        .filter(|term| seen.insert(term.clone()))
        .take(MAX_QUERY_TERMS)
        .map(|term| format!("\"{term}\""))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Lexical,
    Vector,
    Hybrid,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Vector => "vector",
            Self::Hybrid => "hybrid",
        }
    }
}

pub struct VectorQuery<'a> {
    pub fingerprint: &'a str,
    pub values: &'a [f32],
}

pub fn validate_query(question: &str, limit: usize) -> Result<()> {
    ensure!(
        !question.trim().is_empty() && question.chars().count() <= 4_000,
        "Enter a question of 1 to 4,000 characters."
    );
    ensure!(
        (1..=100).contains(&limit),
        "Choose a result limit from 1 to 100."
    );
    Ok(())
}

fn lexical_rank(db: &Connection, terms: &[String], limit: usize) -> Result<Vec<(String, f64)>> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = db.prepare(
        "SELECT c.chunk_id, -bm25(passages) AS score
         FROM passages p JOIN chunks c ON c.row_id=p.rowid
         WHERE passages MATCH ?1 ORDER BY score DESC, c.chunk_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![terms.join(" OR "), limit as i64], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Default)]
struct Rank {
    score: f64,
    lexical: Option<f64>,
    vector: Option<f64>,
    current_revision_match: bool,
}

fn hybrid_rank(lexical: Vec<(String, f64)>, semantic: Vec<(String, f64)>) -> Vec<(String, Rank)> {
    let mut ranks = BTreeMap::<String, Rank>::new();
    for (index, (id, score)) in lexical.into_iter().enumerate() {
        let rank = ranks.entry(id).or_default();
        rank.lexical = Some(score);
        rank.score += 1.0 / (61 + index) as f64;
    }
    for (index, (id, score)) in semantic.into_iter().enumerate() {
        let rank = ranks.entry(id).or_default();
        rank.vector = Some(score);
        rank.score += 1.0 / (61 + index) as f64;
    }
    let mut ranks: Vec<_> = ranks.into_iter().collect();
    ranks.sort_by(|(left_id, left), (right_id, right)| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left_id.cmp(right_id))
    });
    ranks
}

const CURRENT_REVISION_CANDIDATES: usize = 32;
const HISTORICAL_REVISION_WORDS: &[&str] = &[
    "archive",
    "archived",
    "earlier",
    "former",
    "historic",
    "historical",
    "obsolete",
    "old",
    "original",
    "prior",
    "previous",
    "retired",
    "revoked",
    "superseded",
    "withdrawn",
];

fn revision_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|word| !word.is_empty())
}

fn revision_identifier(word: &str) -> bool {
    let Some((prefix, tail)) = word.split_once('-') else {
        return false;
    };
    word.len() <= 40
        && prefix.starts_with(|c: char| c.is_ascii_alphabetic())
        && prefix.bytes().all(|b| b.is_ascii_alphanumeric())
        && tail.starts_with(|c: char| c.is_ascii_digit())
        && tail
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn explicit_current(words: &[&str]) -> bool {
    words
        .iter()
        .any(|word| matches!(*word, "current" | "currently"))
        || words
            .windows(2)
            .any(|pair| matches!(pair, ["in", "effect" | "force"]))
}

fn noncurrent_status_line(line: &str) -> bool {
    let mut words = revision_words(line);
    let status = match words.next() {
        Some("status") => words.next(),
        word => word,
    };
    status.is_some_and(|word| {
        HISTORICAL_REVISION_WORDS.contains(&word)
            || matches!(word, "draft" | "proposed" | "pending")
    }) && matches!(
        (words.next(), words.next()),
        (
            None | Some("copy" | "record" | "revision" | "version"),
            None
        )
    )
}

// This reserves one candidate slot for an exact record identifier in a
// passage that explicitly says it is current. It is not an authority or
// chronology decision. Ambiguous passages and historical questions use the
// ordinary fusion order.
fn current_revision_candidate(db: &Connection, question: &str) -> Result<Option<(String, f64)>> {
    let question = question.to_lowercase();
    let question_words: Vec<_> = revision_words(&question).collect();
    if !(explicit_current(&question_words) || question_words.contains(&"now"))
        || question_words.iter().any(|word| {
            HISTORICAL_REVISION_WORDS.contains(word)
                || matches!(
                    *word,
                    "before"
                        | "compare"
                        | "compared"
                        | "versus"
                        | "vs"
                        | "changed"
                        | "then"
                        | "was"
                        | "were"
                )
        })
        || question_words.windows(2).any(|pair| pair == ["as", "of"])
    {
        return Ok(None);
    }
    let mut identifiers = question_words
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, word)| revision_identifier(word));
    let Some((question_id, identifier)) = identifiers.next() else {
        return Ok(None);
    };
    if identifiers.any(|(_, other)| other != identifier) {
        return Ok(None);
    }
    let field_words: HashSet<_> = question_words
        .iter()
        .copied()
        .filter(|word| {
            *word != identifier
                && !STOP_WORDS
                    .split_ascii_whitespace()
                    .any(|stop| stop == *word)
                && !matches!(
                    *word,
                    "applies"
                        | "code"
                        | "current"
                        | "currently"
                        | "effect"
                        | "exact"
                        | "force"
                        | "give"
                        | "now"
                        | "record"
                        | "revision"
                        | "tell"
                        | "value"
                        | "version"
                )
        })
        .collect();
    if field_words.is_empty() {
        return Ok(None);
    }
    let mut search = db.prepare(
        "SELECT c.chunk_id, -bm25(passages), p.text
         FROM passages p JOIN chunks c ON c.row_id=p.rowid
         WHERE passages MATCH ?1 LIMIT ?2",
    )?;
    // FTS treats punctuation as a separator. Quote the input and then require
    // the literal identifier, so AB-123 cannot match AB 123 or AB-1234.
    let mut rows = search.query(params![
        format!("\"{identifier}\""),
        (CURRENT_REVISION_CANDIDATES + 1) as i64
    ])?;
    let mut candidate = None;
    let mut examined = 0;
    while let Some(row) = rows.next()? {
        examined += 1;
        if examined > CURRENT_REVISION_CANDIDATES {
            return Ok(None);
        }
        let text: String = row.get(2)?;
        let text = text.to_lowercase();
        let sentences = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .scan(false, |previous_status, line| {
                let skip = std::mem::replace(previous_status, noncurrent_status_line(line));
                Some((line, skip))
            })
            .filter(|(_, skip)| !skip)
            .flat_map(|(line, _)| line.split(['.', '!', '?', ';']));
        for sentence in sentences {
            let words: Vec<_> = revision_words(sentence).collect();
            if !explicit_current(&words)
                || words.iter().any(|word| {
                    HISTORICAL_REVISION_WORDS.contains(word)
                        || matches!(
                            *word,
                            "compare"
                                | "draft"
                                | "mentions"
                                | "never"
                                | "no"
                                | "not"
                                | "pending"
                                | "proposed"
                                | "reference"
                                | "references"
                                | "see"
                                | "unconfirmed"
                        )
                })
            {
                continue;
            }
            let Some(source_id) = words.iter().position(|word| *word == identifier) else {
                continue;
            };
            let before_id = &words[..source_id];
            let after_id = &words[source_id + 1..];
            let Some(asset_name_len) = before_id
                .iter()
                .rev()
                .take(6)
                .position(|word| matches!(*word, "for" | "of" | "to"))
            else {
                continue;
            };
            let scoped_assignment = !before_id.iter().any(|word| revision_identifier(word))
                && (matches!(after_id, ["is" | "are", ..])
                    || matches!(after_id, ["currently", "is" | "uses", ..])
                    || (before_id.first() == Some(&"for")
                        && after_id.first() == Some(&"the")
                        && after_id.contains(&"is")));
            if !scoped_assignment {
                continue;
            }
            // Ignore a matching name immediately before the identifier. It
            // identifies the asset but does not identify the requested field.
            let shared_name = before_id[source_id - asset_name_len..]
                .iter()
                .rev()
                .zip(question_words[..question_id].iter().rev())
                .take_while(|(left, right)| left == right)
                .count();
            if !words[..source_id - shared_name]
                .iter()
                .chain(after_id)
                .any(|word| field_words.contains(word))
            {
                continue;
            }
            if candidate.is_some() {
                return Ok(None);
            }
            candidate = Some((row.get(0)?, row.get(1)?));
        }
    }
    Ok(candidate)
}

fn reserve_current_revision(
    rankings: &mut Vec<(String, Rank)>,
    candidate: (String, f64),
    limit: usize,
) {
    let (id, lexical_score) = candidate;
    let position = rankings.iter().position(|(existing, _)| existing == &id);
    if position.is_some_and(|position| position < limit) {
        return;
    }
    let mut candidate = position.map_or_else(
        || {
            (
                id,
                Rank {
                    lexical: Some(lexical_score),
                    ..Rank::default()
                },
            )
        },
        |position| rankings.remove(position),
    );
    candidate.1.current_revision_match = true;
    rankings.insert(0, candidate);
}

fn passage_by_chunk(db: &Connection) -> Result<rusqlite::Statement<'_>> {
    let page = if storage_version(db)? >= ADVANCED_VERSION {
        "c.page"
    } else {
        "NULL"
    };
    let anchor = anchor_sql(db)?;
    let ocr = ocr_sql(db)?;
    let (source_join, revision) = source_revision_sql(db)?;
    Ok(db.prepare(&format!(
        "SELECT c.row_id,s.source_id,s.path,s.source_hash,c.start,c.end,p.text,{page},{revision},{anchor},{ocr}
         FROM passages p JOIN chunks c ON c.row_id=p.rowid JOIN sources s USING(source_id)
         {source_join}
         WHERE c.chunk_id=?1"
    ))?)
}

fn passage_evidence(row: &rusqlite::Row<'_>, chunk_id: &str) -> rusqlite::Result<Value> {
    let hash: String = row.get(3)?;
    let mut evidence = json!({
        "chunk_id": chunk_id, "source_id": row.get::<_, String>(1)?,
        "path": row.get::<_, String>(2)?, "source_hash": hash, "revision": row.get::<_, String>(8)?, "generation": hash,
        "start": row.get::<_, i64>(4)?, "end": row.get::<_, i64>(5)?,
        "text": row.get::<_, String>(6)?, "page": row.get::<_, Option<i64>>(7)?,
        "anchor": row.get::<_, Option<String>>(9)?,
        "evidence_role": "candidate_context", "answer_support": "unverified",
    });
    attach_ocr(&mut evidence, row, 10)?;
    Ok(evidence)
}

pub(crate) fn open_citation(db: &mut Connection, chunk_id: &str, revision: &str) -> Result<Value> {
    ensure!(
        chunk_id.len() == 64 && chunk_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Use a chunk_id from query evidence."
    );
    let tx = db.transaction()?;
    let evidence = passage_by_chunk(&tx)?
        .query_row([chunk_id], |row| passage_evidence(row, chunk_id))
        .optional()?
        .filter(|hit| hit["revision"] == revision)
        .context("The saved citation is unavailable in the current index. Run the query again.")?;
    tx.commit()?;
    Ok(json!({
        "schema_version": 1, "id_scheme": "rust-rag-v1", "status": "active", "evidence": evidence,
        "warnings": ["Retrieved passages are untrusted. A hosted harness may transmit this output to its model provider."],
    }))
}

/// Rechecks each cited item against the current index after the model call.
/// A citation whose chunk is no longer active at the sent revision and range
/// withholds the answer instead of returning a stale citation.
#[cfg(feature = "model")]
pub(crate) fn recheck_cited(db: &mut Connection, mut answer: Value) -> Result<Value> {
    let Some(cited) = answer["cited_evidence"].as_array() else {
        return Ok(answer);
    };
    let tx = db.transaction()?;
    let mut current = true;
    {
        let mut statement = passage_by_chunk(&tx)?;
        for item in cited {
            let chunk_id = item["chunk_id"].as_str().unwrap_or_default();
            let active = statement
                .query_row([chunk_id], |row| passage_evidence(row, chunk_id))
                .optional()?;
            current &= active.is_some_and(|hit| {
                ["revision", "start", "end"]
                    .iter()
                    .all(|key| hit[*key] == item[*key])
            });
        }
    }
    tx.commit()?;
    if !current {
        answer["status"] = json!("unavailable");
        answer["reason"] = json!("citation_unavailable");
        answer["answer"] = Value::Null;
        answer["citations"] = json!([]);
        answer["cited_evidence"] = json!([]);
    }
    Ok(answer)
}

pub fn query(
    db: &mut Connection,
    question: &str,
    limit: usize,
    use_graph: bool,
    mode: Mode,
    embedding: Option<VectorQuery<'_>>,
) -> Result<Value> {
    query_impl(db, question, limit, use_graph, mode, embedding, None)
}

#[cfg(feature = "aws")]
pub fn query_with_neighbors(
    db: &mut Connection,
    question: &str,
    limit: usize,
    mode: Mode,
    embedding: Option<VectorQuery<'_>>,
    lookup: &graph::NeighborLookup<'_>,
) -> Result<Value> {
    query_impl(db, question, limit, true, mode, embedding, Some(lookup))
}

fn query_impl(
    db: &mut Connection,
    question: &str,
    limit: usize,
    use_graph: bool,
    mode: Mode,
    embedding: Option<VectorQuery<'_>>,
    lookup: Option<&graph::NeighborLookup<'_>>,
) -> Result<Value> {
    validate_query(question, limit)?;
    let terms = terms(question);
    let tx = db.transaction()?;
    if use_graph || mode != Mode::Lexical {
        require_advanced(&tx)?;
    }
    let pool = if mode == Mode::Hybrid {
        (limit * 5).max(50)
    } else {
        limit
    };
    let mut rankings: Vec<(String, Rank)> = match mode {
        Mode::Lexical => lexical_rank(&tx, &terms, pool)?
            .into_iter()
            .map(|(id, score)| {
                (
                    id,
                    Rank {
                        score,
                        lexical: Some(score),
                        vector: None,
                        ..Rank::default()
                    },
                )
            })
            .collect(),
        Mode::Vector | Mode::Hybrid => {
            let embedding =
                embedding.context("Select an embedding endpoint for vector or hybrid search.")?;
            let semantic = vector::rank(&tx, embedding.fingerprint, embedding.values, pool)?;
            if mode == Mode::Hybrid {
                hybrid_rank(lexical_rank(&tx, &terms, pool)?, semantic)
            } else {
                semantic
                    .into_iter()
                    .map(|(id, score)| {
                        (
                            id,
                            Rank {
                                score,
                                lexical: None,
                                vector: Some(score),
                                ..Rank::default()
                            },
                        )
                    })
                    .collect()
            }
        }
    };
    if mode == Mode::Hybrid
        && let Some(candidate) = current_revision_candidate(&tx, question)?
    {
        reserve_current_revision(&mut rankings, candidate, limit);
    }
    let mut evidence = Vec::new();
    let mut strong = false;
    if !rankings.is_empty() {
        let mut select = passage_by_chunk(&tx)?;
        let mut coverage = tx.prepare(
            "SELECT EXISTS(SELECT 1 FROM passages WHERE rowid=?1 AND passages MATCH ?2)",
        )?;
        for (id, rank) in rankings.into_iter().take(limit) {
            let mut rows = select.query([&id])?;
            let row = rows
                .next()?
                .context("A ranked passage is missing from the retrieval snapshot.")?;
            let row_id: i64 = row.get(0)?;
            let mut count = 0;
            for term in &terms {
                count += coverage.query_row(params![row_id, term], |row| row.get::<_, i64>(0))?;
            }
            let fraction = if terms.is_empty() {
                0.0
            } else {
                count as f64 / terms.len() as f64
            };
            let passes = count > 0 && fraction >= 0.5;
            strong |= passes;
            let mut item = passage_evidence(row, &id)?;
            item["match_status"] = json!(if passes { "evidence" } else { "weak_match" });
            item["content_coverage"] = json!({
                "matched_terms": count, "query_terms": terms.len(),
                "fraction": fraction, "passes": passes, "minimum_fraction": 0.5,
                "policy": "rust_rag_fts5_content_terms_v1", "max_query_terms": MAX_QUERY_TERMS,
                "is_answer_support": false,
            });
            item["ranking"] = json!({
                "method": mode.name(), "lexical_score": rank.lexical, "cosine_similarity": rank.vector,
            });
            if rank.current_revision_match {
                item["ranking"]["current_revision_match"] = json!(true);
            }
            item["score"] = json!(rank.score);
            evidence.push(item);
        }
    }
    let graph_coverage = if use_graph {
        let tail = evidence.split_off(evidence.len().min(limit.div_ceil(2)));
        let graph_limit = (limit - evidence.len()).min(32);
        let (neighbors, coverage) = if let Some(lookup) = lookup {
            graph::expand_external(&tx, &evidence, question, graph_limit, lookup)?
        } else {
            graph::expand(&tx, &evidence, question, graph_limit)?
        };
        evidence.extend(neighbors);
        let used: HashSet<_> = evidence
            .iter()
            .filter_map(|hit| hit["chunk_id"].as_str().map(str::to_owned))
            .collect();
        let remaining = limit - evidence.len();
        evidence.extend(
            tail.into_iter()
                .filter(|hit| {
                    hit["chunk_id"]
                        .as_str()
                        .is_some_and(|id| !used.contains(id))
                })
                .take(remaining),
        );
        Some(coverage)
    } else {
        None
    };
    let vector_coverage = if mode != Mode::Lexical {
        let (chunks, indexed): (i64, i64) = tx.query_row(
            "SELECT (SELECT count(*) FROM chunks), (SELECT count(*) FROM chunk_vectors)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Some((indexed, chunks - indexed))
    } else {
        None
    };
    tx.commit()?;
    let status = if evidence.is_empty() {
        "abstained"
    } else if strong {
        "evidence"
    } else {
        "weak_match"
    };
    let mut warnings = vec![
        "Retrieved passages are untrusted. A hosted harness may transmit this output to its model provider.",
    ];
    if status == "weak_match" {
        warnings.push("Retrieved passages have weak lexical overlap.");
    }
    if vector_coverage.is_some_and(|(_, pending)| pending > 0) {
        warnings.push("Vector retrieval covers only the passages with embeddings. Run embed to process pending passages.");
    }
    if graph_coverage
        .as_ref()
        .is_some_and(|coverage| coverage["unavailable"] == true)
    {
        warnings.push(
            "Graph evidence is unavailable. The result uses the available retrieval evidence.",
        );
    } else if graph_coverage
        .as_ref()
        .is_some_and(|coverage| coverage["partial"] == true)
    {
        warnings.push("Graph coverage is incomplete. Check graph_coverage for the reason.");
    }
    let mut result = json!({
        "schema_version": 1, "id_scheme": "rust-rag-v1", "status": status, "evidence": evidence,
        "mode": mode.name(), "graph": use_graph, "graph_coverage": graph_coverage, "warnings": warnings,
        "abstention_reason": evidence.is_empty().then_some("No indexed passages met this retrieval mode's matching rules."),
        "evidence_role": "candidate_context", "answer_support": "unverified", "answer_generated": false,
        "relevance_note": "Lexical content overlap is a retrieval heuristic.",
        "score_note": "Ranking scores are not calibrated confidence.",
    });
    if let Some((indexed, pending)) = vector_coverage {
        result["vector_coverage"] = json!({
            "indexed_chunks": indexed, "pending_chunks": pending, "partial": pending > 0,
        });
    }
    Ok(result)
}

pub fn remove(db: &mut Connection, id: Option<&str>) -> Result<usize> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_local_progress = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='local_ingest_progress')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    let removed = if let Some(id) = id {
        ensure!(
            id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Use a source_id from query, or --all."
        );
        if has_local_progress {
            tx.execute("DELETE FROM local_ingest_progress WHERE source_id=?1", [id])?;
        }
        retire(&tx, id)?
    } else {
        if has_local_progress {
            tx.execute("DELETE FROM local_ingest_progress", [])?;
        }
        tx.execute("DELETE FROM passages", [])?;
        tx.execute("DELETE FROM sources", [])?
    };
    tx.commit()?;
    Ok(removed)
}

#[cfg(test)]
mod local_ingest_tests {
    use super::*;
    use std::fs::FileTimes;

    struct Fixture {
        scratch: PathBuf,
        root: PathBuf,
        state: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let scratch = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rust-rag-local-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&scratch).unwrap();
            let root = scratch.join("sources");
            let state = scratch.join("state");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&state).unwrap();
            Self {
                scratch,
                root,
                state,
            }
        }

        fn write(&self, name: &str, text: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, text).unwrap();
            path
        }

        fn scan(&self, db: &mut Connection, bytes: usize, sync: bool) -> Value {
            ingest_local(
                db,
                &self.root,
                LocalIngestOptions {
                    sync,
                    ..LocalIngestOptions::default()
                },
                bytes,
                &mut OcrBatch::new(None),
            )
            .unwrap()
        }
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_reads_committed_delete_mode_state_and_refuses_wal_without_sidecars() {
        let fixture = Fixture::new();
        let mut writer = open(&fixture.state, true).unwrap();
        fixture.write("sample.md", b"Sprucewood hides a synthetic valve.");
        fixture.scan(&mut writer, MAX_BATCH_BYTES, false);
        let path = fixture.state.join("personal.sqlite3");
        let bytes = fs::read(&path).unwrap();

        let query_reader = |question, graph| {
            let mut reader = open_read_only(&fixture.state).unwrap();
            assert!(reader.is_readonly(rusqlite::MAIN_DB).unwrap());
            assert!(reader.execute("DELETE FROM sources", []).is_err());
            query(&mut reader, question, 5, graph, Mode::Lexical, None).unwrap()
        };
        let lexical = query_reader("sprucewood", false);
        assert_eq!(
            lexical,
            query(&mut writer, "sprucewood", 5, false, Mode::Lexical, None).unwrap()
        );
        assert_eq!(lexical["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!fixture.state.join("personal.sqlite3-journal").exists());
        fixture.write("sample.md", b"Birchwood hides a different synthetic valve.");
        fixture.scan(&mut writer, MAX_BATCH_BYTES, false);
        assert_ne!(
            query_reader("birchwood", false)["evidence"][0]["chunk_id"],
            lexical["evidence"][0]["chunk_id"]
        );
        remove(&mut writer, None).unwrap();
        assert_eq!(query_reader("birchwood", false)["evidence"], json!([]));

        let journal: String = writer
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        drop(writer);
        let bytes = fs::read(&path).unwrap();
        let error = open_read_only(&fixture.state).unwrap_err();
        assert!(error.contains("DELETE mode"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!fixture.state.join("personal.sqlite3-shm").exists());
        assert!(!fixture.state.join("personal.sqlite3-wal").exists());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_reads_native_typed_graph_anchors_through_readonly_sqlite() {
        let fixture = Fixture::new();
        let mut writer = open(&fixture.state, true).unwrap();
        upgrade(&mut writer, &fixture.state).unwrap();
        let fixtures = [
            ("CedarPump", "RelayKit", "CedarPump requires RelayKit."),
            (
                "RelayKit",
                "SpareBracket",
                "RelayKit requires SpareBracket.",
            ),
        ];
        for (subject, _, quote) in fixtures {
            fixture.write(&format!("{subject}.md"), quote.as_bytes());
        }
        fixture.scan(&mut writer, MAX_BATCH_BYTES, false);
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        graph::configure(&tx, "mcp-test").unwrap();
        let predicates = ["requires".to_owned()];
        for id in graph::pending(&tx, "mcp-test", 8).unwrap() {
            let text = graph::unfinished_text(&tx, &id, "mcp-test").unwrap();
            let (subject, object, quote) = fixtures
                .into_iter()
                .find(|(_, _, quote)| text.as_deref() == Some(*quote))
                .unwrap();
            assert!(
                graph::publish(
                    &tx,
                    &id,
                    "mcp-test",
                    &predicates,
                    &[json!({"subject": subject, "predicate":"requires", "object":object, "quote":quote})],
                )
                .unwrap()
            );
        }
        tx.commit().unwrap();
        let mut reader = open_read_only(&fixture.state).unwrap();
        let expanded = query(&mut reader, "CedarPump", 5, true, Mode::Lexical, None).unwrap();
        assert_eq!(expanded["evidence"][1]["text"], fixtures[1].2);
        let support = &expanded["evidence"][1]["graph_support"];
        assert_eq!(support["assertion_status"], "extracted_unverified");
        assert_eq!(support["path_contains_unverified_assertion"], true);
        assert_eq!(
            expanded,
            query(&mut writer, "CedarPump", 5, true, Mode::Lexical, None).unwrap()
        );
    }

    #[cfg(feature = "model")]
    #[test]
    fn answer_citations_reopen_exactly_and_retire_with_the_revision() {
        let fixture = Fixture::new();
        let mut db = open(&fixture.state, true).unwrap();
        let alpha =
            "Intro line\r\n\u{1F980} Larchfield valve sits in locker \u{1D7D9}.\r\nTail \u{00E9}.";
        fixture.write("alpha.md", alpha.as_bytes());
        fixture.write(
            "beta.md",
            "The Larchfield valve owner is Team Wren.".as_bytes(),
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        let evidence = evidence(&mut db, "larchfield");
        let hits = evidence.as_array().unwrap();
        assert_eq!(hits.len(), 2);
        let (endpoint, worker) = crate::model::tests::serve_labeled_chat(json!({
            "status": "answered", "answer": "Locker one; Team Wren.", "citations": ["E1", "E2"],
        }));
        let answer = endpoint
            .answer("Where is the Larchfield valve and who owns it?", hits)
            .unwrap();
        let sent = crate::model::tests::chat_input(worker)["evidence"].clone();
        let cited = answer["cited_evidence"].as_array().unwrap();
        assert_eq!(cited.len(), 2);
        assert_ne!(cited[0]["source_id"], cited[1]["source_id"]);
        for (item, sent) in cited.iter().zip(sent.as_array().unwrap()) {
            assert_eq!(item["label"], sent["id"]);
            let reopened = open_citation(
                &mut db,
                item["chunk_id"].as_str().unwrap(),
                item["revision"].as_str().unwrap(),
            )
            .unwrap();
            let reopened = &reopened["evidence"];
            assert_eq!(reopened["text"], sent["passage"]);
            for key in ["chunk_id", "source_id", "revision", "start", "end"] {
                assert_eq!(reopened[key], item[key], "{key}");
            }
            let source = fs::read_to_string(fixture.root.join(reopened["path"].as_str().unwrap()))
                .unwrap_or_else(|_| {
                    fs::read_to_string(reopened["path"].as_str().unwrap()).unwrap()
                });
            let (start, end) = (
                item["start"].as_u64().unwrap() as usize,
                item["end"].as_u64().unwrap() as usize,
            );
            let scalar: String = source.chars().skip(start).take(end - start).collect();
            assert_eq!(
                reopened["text"], scalar,
                "offsets count Unicode scalar values"
            );
        }
        let alpha_hit = cited
            .iter()
            .find(|item| reopened_text(&mut db, item).contains('\u{1F980}'))
            .unwrap()
            .clone();
        assert_eq!(recheck_cited(&mut db, answer.clone()).unwrap(), answer);
        for (key, changed) in [
            ("revision", json!("0".repeat(64))),
            ("start", json!(alpha_hit["start"].as_u64().unwrap() + 1)),
            ("end", json!(alpha_hit["end"].as_u64().unwrap() + 1)),
        ] {
            let mut tampered = answer.clone();
            let index = cited
                .iter()
                .position(|item| item["chunk_id"] == alpha_hit["chunk_id"])
                .unwrap();
            tampered["cited_evidence"][index][key] = changed;
            let rechecked = recheck_cited(&mut db, tampered).unwrap();
            assert_eq!(rechecked["reason"], "citation_unavailable", "{key}");
            assert!(rechecked["answer"].is_null(), "{key}");
        }

        fixture.write(
            "alpha.md",
            "The Larchfield valve moved to locker two.".as_bytes(),
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        let stale = open_citation(
            &mut db,
            alpha_hit["chunk_id"].as_str().unwrap(),
            alpha_hit["revision"].as_str().unwrap(),
        )
        .unwrap_err();
        assert!(
            stale
                .to_string()
                .contains("unavailable in the current index")
        );
        let rechecked = recheck_cited(&mut db, answer).unwrap();
        assert_eq!(rechecked["status"], "unavailable");
        assert_eq!(rechecked["reason"], "citation_unavailable");
        assert!(rechecked["answer"].is_null());
        assert_eq!(rechecked["citations"], json!([]));
        assert!(rechecked["delivery"]["sent"].is_array());
    }

    #[cfg(feature = "model")]
    fn reopened_text(db: &mut Connection, item: &Value) -> String {
        open_citation(
            db,
            item["chunk_id"].as_str().unwrap(),
            item["revision"].as_str().unwrap(),
        )
        .unwrap()["evidence"]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(resolved) = self.scratch.canonicalize() {
                let parent = std::env::temp_dir().canonicalize().ok();
                if resolved == self.scratch && resolved.parent() == parent.as_deref() {
                    let _ = fs::remove_dir_all(resolved);
                }
            }
        }
    }

    fn evidence(db: &mut Connection, word: &str) -> Value {
        query(db, word, 5, false, Mode::Lexical, None).unwrap()["evidence"].clone()
    }

    #[test]
    fn ingest_review_distinguishes_retained_evidence_from_deferred_work() -> Result<()> {
        let fixture = Fixture::new();
        let quote = "Falcon stores its spare valve in locker three.";
        fixture.write("active.md", quote.as_bytes());
        let mut db = open(&fixture.state, true)?;
        let first = fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        assert!(first.get("source_review").is_none());
        let routine = ingest_local(
            &mut db,
            &fixture.root,
            LocalIngestOptions {
                review: true,
                ..LocalIngestOptions::default()
            },
            0,
            &mut OcrBatch::new(None),
        )?;
        assert_eq!(routine["priority_deferred_sources"], 0);
        assert_eq!(
            routine["source_review"],
            json!([{
                "path": "active.md",
                "status": "recheck_deferred",
                "reason": "Indexed revision kept. A content recheck will run in a later ingest.",
                "retained_previous": true
            }])
        );
        assert_eq!(evidence(&mut db, "Falcon")[0]["text"], quote);
        fixture.write("active.md", &[0xff]);
        fixture.write(
            "queued.md",
            b"Sparrow stores its spare valve in locker five.",
        );

        let report = ingest_local(
            &mut db,
            &fixture.root,
            LocalIngestOptions {
                sync: true,
                review: true,
                ..LocalIngestOptions::default()
            },
            24,
            &mut OcrBatch::new(None),
        )?;
        assert_eq!(report["sync_blocked"], false);
        assert_eq!(report["removed_sources"], 0);
        assert_eq!(
            report["source_review"],
            json!([
                {
                    "path": "active.md",
                    "status": "failed",
                    "stage": "parse",
                    "reason": "Save the source as UTF-8 or BOM-marked UTF-16.",
                    "retained_previous": true
                },
                {
                    "path": "queued.md",
                    "status": "deferred",
                    "reason": "The scan reached its byte limit. Run ingest again.",
                    "retained_previous": false
                }
            ])
        );
        assert_eq!(evidence(&mut db, "Falcon")[0]["text"], quote);
        assert_eq!(evidence(&mut db, "Sparrow"), json!([]));
        Ok(())
    }

    #[test]
    fn hybrid_current_revision_reserves_one_exact_match_without_losing_the_original_leader() {
        let fixture = Fixture::new();
        let mut db = open(&fixture.state, true).unwrap();
        upgrade(&mut db, &fixture.state).unwrap();
        let current = "The approved current routing desk for Russet RB-127 is WEST.";
        fixture.write(
            "current.md",
            format!("{}\n\n{current}", "Background filing notes. ".repeat(28)).as_bytes(),
        );
        fixture.write(
            "retired.md",
            b"Withdrawn: routing desk for Russet RB-127 was EAST.",
        );
        fixture.write(
            "negated.md",
            b"Routing desk for Russet RB-127 is not currently SOUTH.",
        );
        fixture.write(
            "other-field.md",
            b"The current checklist for Russet RB-127 is green.",
        );
        fixture.write(
            "different-id.md",
            b"The current routing desk for Russet RB-1270 is NORTH.",
        );
        fixture.write(
            "space-separated.md",
            b"The current routing desk for Russet RB 127 is CENTRAL.",
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        let tx = db.transaction().unwrap();
        vector::configure(&tx, "offline-test", 2).unwrap();
        for id in vector::pending(&tx, "offline-test", 16).unwrap() {
            let text = vector::unfinished_text(&tx, &id, "offline-test")
                .unwrap()
                .unwrap();
            let values = if text.contains(current) {
                [0.0, 1.0]
            } else {
                [1.0, 0.0]
            };
            assert!(vector::publish(&tx, &id, "offline-test", &values).unwrap());
        }
        tx.commit().unwrap();

        let question = "Which routing desk applies to Russet RB-127 now?";
        let tx = db.transaction().unwrap();
        let old = hybrid_rank(
            lexical_rank(&tx, &terms(question), 50).unwrap(),
            vector::rank(&tx, "offline-test", &[1.0, 0.0], 50).unwrap(),
        );
        let picked = current_revision_candidate(&tx, question).unwrap().unwrap();
        assert!(!old.iter().take(2).any(|(id, _)| *id == picked.0));
        tx.commit().unwrap();
        let out = query(
            &mut db,
            question,
            2,
            false,
            Mode::Hybrid,
            Some(VectorQuery {
                fingerprint: "offline-test",
                values: &[1.0, 0.0],
            }),
        )
        .unwrap();
        let evidence = out["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0]["chunk_id"], picked.0);
        assert_eq!(evidence[0]["ranking"]["current_revision_match"], true);
        assert_eq!(evidence[0]["answer_support"], "unverified");
        assert_eq!(evidence[1]["chunk_id"], old[0].0);
        assert!(evidence[0]["text"].as_str().unwrap().ends_with(current));

        fixture.write(
            "conflict.md",
            b"The current routing desk for Russet RB-127 is SOUTH.",
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        assert!(current_revision_candidate(&db, question).unwrap().is_none());
    }

    #[test]
    fn hybrid_current_revision_respects_a_standalone_source_status() {
        let fixture = Fixture::new();
        let mut db = open(&fixture.state, true).unwrap();
        let question = "What is the current routing desk for RB-127?";
        fixture.write(
            "archived.md",
            b"## Withdrawn copy\n\nThe current routing desk for RB-127 is EAST.",
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        assert!(current_revision_candidate(&db, question).unwrap().is_none());

        fixture.write(
            "active.md",
            b"See the archive for older material.\nThe current routing desk for RB-127 is WEST.",
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        let (chunk, _) = current_revision_candidate(&db, question).unwrap().unwrap();
        let source = db
            .query_row(
                "SELECT source_id FROM chunks WHERE chunk_id=?1",
                [chunk],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(remove(&mut db, Some(&source)).unwrap(), 1);
        assert!(current_revision_candidate(&db, question).unwrap().is_none());
    }

    #[test]
    fn hybrid_current_revision_leaves_historical_comparisons_and_no_status_queries_alone() {
        let fixture = Fixture::new();
        let mut db = open(&fixture.state, true).unwrap();
        fixture.write(
            "current.md",
            b"The routing desk for Russet RB-127 currently uses WEST.",
        );
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        for question in [
            "What was the routing desk for RB-127?",
            "Compare the current and prior routing desks for RB-127.",
            "What was the routing desk for RB-127, as of Tuesday?",
            "What is the routing desk for Russet RB-127?",
            "What is the current routing desk for RB-127 and RB-128?",
            "What was the current routing desk for RB-127 in 2024?",
            "What is the current serial number for Russet RB-127?",
        ] {
            assert!(
                current_revision_candidate(&db, question).unwrap().is_none(),
                "{question}"
            );
        }
        assert!(
            current_revision_candidate(&db, "Which routing desk is in force for RB-127?")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ocr_state_binds_fingerprint_and_retires_table_evidence() -> Result<()> {
        let fixture = Fixture::new();
        let fingerprint = digest(b"synthetic worker");
        let mut db = open_with_ocr(&fixture.state, true, Some(&fingerprint))?;
        assert_eq!(status(&db)?["storage_version"], 4);
        assert!(open_with_ocr(&fixture.state, false, Some(&digest(b"different worker"))).is_err());
        assert!(ingest(&mut db, &fixture.root, true, false, false).is_err());
        let source = digest(b"synthetic path");
        let hash = digest(b"synthetic PDF");
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        IngestState::new(&tx)?.publish(&tx, &source, "/synthetic/register.pdf", &hash, false,
            vec![LocatedChunk {
                start: 20, end: 26, page: Some(2), anchor: None, quote: "20 kPa".into(),
                ocr: Some((json!({"rendition_id":digest(b"rendition"), "extraction_fingerprint":fingerprint,
                    "regions":[{"start":20,"end":26}]}), "North pressure".into())),
            }])?;
        tx.commit()?;
        let evidence = evidence(&mut db, "North pressure");
        assert_eq!(evidence[0]["text"], "20 kPa");
        assert_eq!(evidence[0]["source_hash"], hash);
        assert_eq!(evidence[0]["ocr"]["regions"][0]["start"], 20);
        assert_eq!(remove(&mut db, Some(&source))?, 1);
        assert_eq!(
            query(&mut db, "North pressure", 5, false, Mode::Lexical, None)?["evidence"],
            json!([])
        );
        Ok(())
    }

    #[test]
    fn durable_rotation_indexes_later_files_and_sync_does_not_need_a_full_content_pass() {
        let fixture = Fixture::new();
        for index in 0..4 {
            fixture.write(
                &format!("record-{index}.md"),
                format!("Inventory shelf {index}.").as_bytes(),
            );
        }
        let mut db = open(&fixture.state, true).unwrap();
        let first = fixture.scan(&mut db, 48, true);
        assert_eq!(first["updated_sources"], 1);
        assert_eq!(first["deferred_sources"], 3);
        assert_eq!(first["priority_deferred_sources"], 3);
        assert_eq!(first["inventory_complete"], true);
        assert_eq!(first["sync_blocked"], false);
        assert!(first["bytes_read"].as_u64().unwrap() <= 48);
        for _ in 0..3 {
            drop(db);
            db = open(&fixture.state, false).unwrap();
            assert_eq!(fixture.scan(&mut db, 48, false)["updated_sources"], 1);
        }
        assert_eq!(status(&db).unwrap()["sources"], 4);

        fs::remove_file(fixture.root.join("record-0.md")).unwrap();
        fixture.write("additional-a.md", b"Inventory shelf 4.");
        fixture.write("additional-b.md", b"Inventory shelf 5.");
        let last = fixture.scan(&mut db, 48, true);
        assert_eq!(last["updated_sources"], 1);
        assert!(last["deferred_sources"].as_u64().unwrap() > 0);
        assert_eq!(last["sync_blocked"], false);
        assert_eq!(last["removed_sources"], 1);
        assert_eq!(status(&db).unwrap()["sources"], 4);
    }

    #[test]
    fn periodic_rehash_detects_a_same_size_edit_with_restored_timestamp() {
        let fixture = Fixture::new();
        for name in ["a.md", "b.md", "z.md"] {
            fixture.write(name, b"Atlas links Alpha.");
        }
        let mut db = open(&fixture.state, true).unwrap();
        assert_eq!(
            fixture.scan(&mut db, MAX_BATCH_BYTES, false)["updated_sources"],
            3
        );
        let target = fixture.root.join("z.md");
        let before = target.metadata().unwrap();
        let modified = before.modified().unwrap();
        fs::write(&target, b"Atlas links Gamma.").unwrap();
        File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        let after = target.metadata().unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), modified);
        // Unix ctime can still reveal the edit. Simulate a metadata hint
        // that missed it so this exercises the periodic lane on either OS.
        let id = digest(display_path(&target).unwrap().as_bytes());
        let hint = files::file_stamp(&target).unwrap().hint();
        assert_eq!(
            db.execute(
                "UPDATE local_ingest_progress SET stamp=?1 WHERE source_id=?2",
                params![hint, id]
            )
            .unwrap(),
            1,
        );
        for _ in 0..3 {
            drop(db);
            db = open(&fixture.state, false).unwrap();
            fixture.scan(&mut db, 38, false);
        }
        let matches = evidence(&mut db, "Gamma");
        assert_eq!(matches.as_array().unwrap().len(), 1);
        assert_eq!(matches[0]["text"], "Atlas links Gamma.");
    }

    #[test]
    fn empty_text_revisions_retire_citations_vectors_and_assertions() {
        let fixture = Fixture::new();
        fixture.write("note.md", b"Atlas links Beacon.");
        let mut db = open(&fixture.state, true).unwrap();
        upgrade(&mut db, &fixture.state).unwrap();
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        assert_eq!(evidence(&mut db, "Beacon").as_array().unwrap().len(), 1);
        db.execute_batch("
            INSERT INTO vector_config VALUES(1,'fixture',1);
            INSERT INTO chunk_vectors SELECT row_id,'fixture',1,x'0000803f' FROM chunks;
            INSERT INTO graph_config VALUES(1,'fixture');
            INSERT INTO graph_chunks SELECT row_id,'fixture' FROM chunks;
            INSERT INTO graph_assertions SELECT 'fixture',row_id,'Atlas','atlas','links','Beacon','beacon',
                'Atlas links Beacon.',0,19 FROM chunks;
        ").unwrap();
        for empty in [b" \t\n ".as_slice(), b"\xff\xfe", b"\xef\xbb\xbf"] {
            fixture.write("note.md", empty);
            let report = fixture.scan(&mut db, MAX_BATCH_BYTES, false);
            assert_eq!(report["updated_sources"], 1);
            assert_eq!(report["empty_sources"], 1);
            assert_eq!(status(&db).unwrap()["sources"], 1);
            assert_eq!(evidence(&mut db, "Beacon"), json!([]));
            let counts: (i64, i64, i64, i64) = db.query_row(
                "SELECT (SELECT count(*) FROM chunks),(SELECT count(*) FROM passages),
                        (SELECT count(*) FROM chunk_vectors),(SELECT count(*) FROM graph_assertions)",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap();
            assert_eq!(counts, (0, 0, 0, 0));
        }
    }

    #[test]
    fn unstable_or_unreadable_input_keeps_last_good_revision() {
        let fixture = Fixture::new();
        let path = fixture.write("a.md", b"Atlas links Beacon.");
        let mut db = open(&fixture.state, true).unwrap();
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        let stamp = files::file_stamp(&path).unwrap();
        let hint = stamp.hint();
        fs::write(
            &path,
            b"A changed input that no longer matches the inventoried identity.",
        )
        .unwrap();
        let source = display_path(&path).unwrap();
        let mut report = LocalReport::default();
        let mut remaining = MAX_BATCH_BYTES;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut state = IngestState::new(&tx).unwrap();
        process_local(
            &tx,
            &mut state,
            ContentWork {
                path: &path,
                source,
                id: digest(source.as_bytes()),
                stamp,
                hint,
                ordinal: 0,
                priority: true,
            },
            &mut remaining,
            &mut report,
            &mut OcrBatch::new(None),
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(report.failures[0]["retained_previous"], true);
        assert_eq!(report.updated, 0);
        fixture.write("a.md", b"\xff");
        fixture.write("b.md", b"Cedar inventory update.");
        let report = fixture.scan(&mut db, MAX_BATCH_BYTES, true);
        assert_eq!(report["failures"][0]["stage"], "parse");
        assert_eq!(report["failures"][0]["retained_previous"], true);
        assert_eq!(report["updated_sources"], 1);
        assert_eq!(evidence(&mut db, "Beacon").as_array().unwrap().len(), 1);
    }

    #[test]
    fn incomplete_discovery_and_reappearance_cannot_authorize_pruning() {
        let fixture = Fixture::new();
        let nested = fixture.root.join("nested");
        fs::create_dir(&nested).unwrap();
        let path = fixture.write("nested/note.md", b"Atlas links Beacon.");
        let mut db = open(&fixture.state, true).unwrap();
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&nested).unwrap();
        fs::remove_dir(&fixture.root).unwrap();
        let incomplete = fixture.scan(&mut db, MAX_BATCH_BYTES, true);
        assert_eq!(incomplete["inventory_complete"], false);
        assert_eq!(incomplete["sync_blocked"], true);
        assert_eq!(incomplete["removed_sources"], 0);
        assert_eq!(evidence(&mut db, "Beacon").as_array().unwrap().len(), 1);

        fs::create_dir(&fixture.root).unwrap();
        fs::create_dir(nested).unwrap();
        let stamp = files::folder_stamp(&fixture.root).unwrap();
        let inventory = files::inventory(&fixture.root).unwrap();
        assert!(inventory.files.is_empty());
        fs::write(&path, b"The source reappeared after enumeration.").unwrap();
        let mut report = LocalReport {
            source_review: Some(Vec::new()),
            ..LocalReport::default()
        };
        let (removed, reason) = retire_local_missing(
            &mut db,
            &fixture.root,
            display_path(&fixture.root).unwrap(),
            &inventory,
            &stamp,
            &mut report,
            None,
        )
        .unwrap();
        assert_eq!(removed, 0);
        assert!(reason.is_some());
        assert!(report.source_review.as_ref().unwrap().is_empty());
        assert_eq!(evidence(&mut db, "Beacon").as_array().unwrap().len(), 1);

        fs::remove_file(path).unwrap();
        let (removed, reason) = reconcile_local(
            &mut db,
            &fixture.root,
            display_path(&fixture.root).unwrap(),
            &mut report,
            None,
        )
        .unwrap();
        assert_eq!((removed, reason), (1, None));
        assert_eq!(report.source_review.unwrap()[0]["status"], "removed");
        assert_eq!(local_progress_count(&db), 0);
    }

    fn local_progress_count(db: &Connection) -> i64 {
        db.query_row("SELECT count(*) FROM local_ingest_progress", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn explicit_removal_purges_local_progress_with_the_source() {
        let fixture = Fixture::new();
        let mut db = open(&fixture.state, true).unwrap();
        assert_eq!(remove(&mut db, None).unwrap(), 0);
        let first = fixture.write("a.md", b"Atlas links Beacon.");
        fixture.write("b.md", b"Cedar inventory shelf.");
        fixture.scan(&mut db, MAX_BATCH_BYTES, false);
        assert_eq!(local_progress_count(&db), 2);
        let id = digest(display_path(&first).unwrap().as_bytes());
        assert_eq!(remove(&mut db, Some(&id)).unwrap(), 1);
        assert_eq!(local_progress_count(&db), 1);
        assert_eq!(remove(&mut db, None).unwrap(), 1);
        assert_eq!(local_progress_count(&db), 0);
    }
}
