//! Owner-local, bounded exact vector ranking over active evidence chunks.

use anyhow::{Context, Result, ensure};
#[cfg(any(feature = "model", test))]
use rusqlite::params;
use rusqlite::{Connection, OptionalExtension};
use std::{cmp::Ordering, collections::BinaryHeap};

const MAX_DIMENSIONS: usize = 8_192;
const MAX_SCAN_ROWS: usize = 100_000;
const MAX_SCAN_BYTES: usize = 128 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
const MIN_SIMILARITY: f64 = 0.2;
const FLOAT_BYTES: usize = size_of::<f32>();
#[cfg(test)]
const MAX_BATCH: usize = 256;

pub const SCHEMA: &str = "
CREATE TABLE vector_config (
    id INTEGER PRIMARY KEY CHECK (id=1),
    fingerprint TEXT NOT NULL,
    dimensions INTEGER NOT NULL CHECK (dimensions BETWEEN 1 AND 8192),
    UNIQUE (fingerprint, dimensions)
);
CREATE TABLE chunk_vectors (
    chunk_row INTEGER PRIMARY KEY REFERENCES chunks(row_id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    embedding BLOB NOT NULL CHECK (typeof(embedding)='blob' AND length(embedding)=dimensions*4),
    FOREIGN KEY (fingerprint,dimensions) REFERENCES vector_config(fingerprint,dimensions) ON DELETE CASCADE
);
CREATE INDEX chunk_vectors_by_model ON chunk_vectors(fingerprint,dimensions);
";

fn valid_dimensions(dimensions: usize) -> Result<()> {
    ensure!(
        (1..=MAX_DIMENSIONS).contains(&dimensions),
        "Embeddings need 1 to {MAX_DIMENSIONS} dimensions."
    );
    Ok(())
}

/// Resolve the index configuration before sending a query to an embedding endpoint.
/// Ranking repeats the check in its read snapshot.
pub fn require_model(db: &Connection, fingerprint: &str, dimensions: usize) -> Result<()> {
    valid_dimensions(dimensions)?;
    let active: Option<(String, i64)> = db
        .query_row(
            "SELECT fingerprint,dimensions FROM vector_config WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    ensure!(
        active.is_some_and(
            |(configured, count)| configured == fingerprint && count == dimensions as i64
        ),
        "The index does not use this embedding model and dimension. Index with the selected model first."
    );
    Ok(())
}

/// Select the active model in the caller's write transaction. Changing models
/// retires the prior vectors while preserving the source passages.
#[cfg(any(feature = "model", test))]
pub fn configure(db: &Connection, fingerprint: &str, dimensions: usize) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "Vector configuration requires a write transaction."
    );
    valid_dimensions(dimensions)?;
    ensure!(
        !fingerprint.is_empty() && fingerprint.len() <= 512,
        "The embedding fingerprint is missing or too long."
    );
    db.execute(
        "DELETE FROM vector_config WHERE fingerprint<>?1",
        [fingerprint],
    )?;
    db.execute(
        "INSERT INTO vector_config(id,fingerprint,dimensions) VALUES(1,?1,?2) ON CONFLICT(id) DO NOTHING",
        params![fingerprint, dimensions as i64],
    )?;
    require_model(db, fingerprint, dimensions)
}

/// Capture bounded IDs rather than sending text from a stale batch snapshot.
#[cfg(test)]
pub fn pending(db: &Connection, fingerprint: &str, limit: usize) -> Result<Vec<String>> {
    ensure!(
        (1..=MAX_BATCH).contains(&limit),
        "Choose 1 to {MAX_BATCH} passages."
    );
    let mut statement = db.prepare(
        "SELECT c.chunk_id
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN vector_config m ON m.id=1 AND m.fingerprint=?1
         LEFT JOIN chunk_vectors v ON v.chunk_row=c.row_id
         WHERE v.chunk_row IS NULL ORDER BY c.row_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![fingerprint, limit as i64], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(any(feature = "model", test))]
pub fn unfinished_text(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT p.text
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN vector_config m ON m.id=1 AND m.fingerprint=?2
         LEFT JOIN chunk_vectors v ON v.chunk_row=c.row_id
         WHERE c.chunk_id=?1 AND v.chunk_row IS NULL",
            params![chunk_id, fingerprint],
            |row| row.get(0),
        )
        .optional()?)
}

fn norm(values: &[f32]) -> Result<f64> {
    valid_dimensions(values.len())?;
    let mut squares = 0.0;
    for &value in values {
        ensure!(
            value.is_finite(),
            "Embeddings must contain only finite values."
        );
        squares += f64::from(value).powi(2);
    }
    ensure!(squares > 0.0, "An embedding must have nonzero length.");
    Ok(squares.sqrt())
}

/// Publish only if the same model and unfinished chunk still exist after
/// the embedding call. A concurrent completion takes precedence.
#[cfg(any(feature = "model", test))]
pub fn publish(db: &Connection, chunk_id: &str, fingerprint: &str, values: &[f32]) -> Result<bool> {
    ensure!(
        !db.is_autocommit(),
        "Vector publication requires a write transaction."
    );
    let target: Option<(i64, i64)> = db
        .query_row(
            "SELECT c.row_id,m.dimensions
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN vector_config m ON m.id=1 AND m.fingerprint=?2
         LEFT JOIN chunk_vectors v ON v.chunk_row=c.row_id
         WHERE c.chunk_id=?1 AND v.chunk_row IS NULL",
            params![chunk_id, fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((row_id, dimensions)) = target else {
        return Ok(false);
    };
    ensure!(
        values.len() as i64 == dimensions,
        "The embedding dimension differs from the index configuration."
    );
    norm(values)?;
    let encoded: Vec<_> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    db.execute(
        "INSERT INTO chunk_vectors(chunk_row,fingerprint,dimensions,embedding) VALUES(?1,?2,?3,?4)",
        params![row_id, fingerprint, dimensions, encoded],
    )?;
    Ok(true)
}

fn validate_scan(count: usize, dimensions: usize) -> Result<()> {
    ensure!(
        count <= MAX_SCAN_ROWS && count <= MAX_SCAN_BYTES / (dimensions * FLOAT_BYTES),
        "The exact vector index exceeds the {MAX_SCAN_ROWS}-passage or 128 MiB search limit."
    );
    Ok(())
}

fn cosine(encoded: &[u8], query: &[f32], query_norm: f64) -> Result<f64> {
    ensure!(
        encoded.len() == query.len() * FLOAT_BYTES,
        "An indexed embedding has the wrong size. Reindex this source."
    );
    let (mut dot, mut squares) = (0.0, 0.0);
    for (bytes, &query_value) in encoded.as_chunks::<FLOAT_BYTES>().0.iter().zip(query) {
        let value = f32::from_le_bytes(*bytes);
        ensure!(
            value.is_finite(),
            "An indexed embedding is invalid. Reindex this source."
        );
        dot += f64::from(value) * f64::from(query_value);
        squares += f64::from(value).powi(2);
    }
    ensure!(
        squares > 0.0,
        "An indexed embedding is invalid. Reindex this source."
    );
    Ok((dot / (query_norm * squares.sqrt())).clamp(-1.0, 1.0))
}

struct Hit {
    chunk: String,
    score: f64,
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        // Keep the worst retained result at the top of the max-heap.
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.chunk.cmp(&other.chunk))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Hit {}

/// Stream embeddings from one snapshot, with one vector and at most `limit`
/// ranked IDs in memory. The byte bound applies to the whole exact scan.
pub fn rank(
    db: &Connection,
    fingerprint: &str,
    query: &[f32],
    limit: usize,
) -> Result<Vec<(String, f64)>> {
    ensure!(
        !db.is_autocommit(),
        "Vector ranking requires the retrieval snapshot."
    );
    ensure!(
        (1..=MAX_RESULTS).contains(&limit),
        "Choose 1 to {MAX_RESULTS} vector results."
    );
    require_model(db, fingerprint, query.len())?;
    let query_norm = norm(query)?;
    let count: i64 = db.query_row("SELECT count(*) FROM chunk_vectors", [], |row| row.get(0))?;
    validate_scan(usize::try_from(count)?, query.len())?;

    let dimensions = query.len() as i64;
    let mut statement = db.prepare(
        "SELECT c.chunk_id,v.fingerprint,v.dimensions,
                CASE WHEN typeof(v.embedding)='blob' AND length(v.embedding)=?1 THEN v.embedding END
         FROM chunk_vectors v LEFT JOIN chunks c ON c.row_id=v.chunk_row",
    )?;
    let mut rows = statement.query([(query.len() * FLOAT_BYTES) as i64])?;
    let mut best = BinaryHeap::with_capacity(limit);
    while let Some(row) = rows.next()? {
        let chunk: String = row
            .get(0)
            .context("An embedding is detached from its source. Reindex this source.")?;
        let saved_fingerprint: String = row.get(1)?;
        let saved_dimensions: i64 = row.get(2)?;
        ensure!(
            saved_fingerprint == fingerprint && saved_dimensions == dimensions,
            "An indexed embedding does not match the active model. Reindex this source."
        );
        let encoded = row
            .get_ref(3)?
            .as_blob()
            .context("An indexed embedding has the wrong size or type. Reindex this source.")?;
        let score = cosine(encoded, query, query_norm)?;
        if score >= MIN_SIMILARITY {
            let hit = Hit { chunk, score };
            if best.len() < limit {
                best.push(hit);
            } else if let Some(mut worst) = best.peek_mut()
                && hit < *worst
            {
                *worst = hit;
            }
        }
    }
    Ok(best
        .into_sorted_vec()
        .into_iter()
        .map(|hit| (hit.chunk, hit.score))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::TransactionBehavior::Immediate;

    fn index() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE sources(source_id TEXT PRIMARY KEY);
             CREATE TABLE chunks(
                row_id INTEGER PRIMARY KEY,chunk_id TEXT UNIQUE,
                source_id TEXT REFERENCES sources(source_id) ON DELETE CASCADE
             );
             CREATE TABLE passages(text TEXT);",
        )
        .unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }

    fn add(db: &Connection, chunk: &str) {
        db.execute("INSERT INTO sources VALUES(?1)", [chunk])
            .unwrap();
        db.execute(
            "INSERT INTO chunks(chunk_id,source_id) VALUES(?1,?1)",
            [chunk],
        )
        .unwrap();
        db.execute(
            "INSERT INTO passages(rowid,text) VALUES(?1,?2)",
            params![db.last_insert_rowid(), chunk],
        )
        .unwrap();
    }

    #[test]
    fn ranks_with_stable_ties_and_skips_low_similarity() {
        let mut db = index();
        let tx = db.transaction_with_behavior(Immediate).unwrap();
        configure(&tx, "model", 2).unwrap();
        for (id, values) in [
            ("z", [3.0, 0.0]),
            ("a", [2.0, 0.0]),
            ("b", [1.0, 1.0]),
            ("c", [0.0, 1.0]),
        ] {
            add(&tx, id);
            assert!(publish(&tx, id, "model", &values).unwrap());
        }
        let best = rank(&tx, "model", &[2.0, 0.0], 2).unwrap();
        assert_eq!(best, [("a".into(), 1.0), ("z".into(), 1.0)]);
        assert_eq!(
            rank(&tx, "model", &[2.0, 0.0], 1).unwrap(),
            [("a".into(), 1.0)]
        );
        let matches = rank(&tx, "model", &[1.0, 0.0], 500).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[2].0, "b");
        assert!((matches[2].1 - 2_f64.sqrt().recip()).abs() < 1e-12);
        assert!(rank(&tx, "different", &[1.0, 0.0], 1).is_err());
        assert!(rank(&tx, "model", &[1.0], 1).is_err());
        assert!(rank(&tx, "model", &[0.0, 0.0], 1).is_err());
        assert!(rank(&tx, "model", &[1.0, f32::INFINITY], 1).is_err());
        tx.commit().unwrap();
    }

    #[test]
    fn model_switch_and_deletion_reject_late_publication() {
        let mut db = index();
        let tx = db.transaction_with_behavior(Immediate).unwrap();
        configure(&tx, "before", 2).unwrap();
        add(&tx, "a");
        add(&tx, "b");
        assert_eq!(pending(&tx, "before", 2).unwrap(), ["a", "b"]);
        assert_eq!(
            unfinished_text(&tx, "a", "before").unwrap().as_deref(),
            Some("a")
        );
        assert!(publish(&tx, "a", "before", &[f32::NAN, 0.0]).is_err());
        assert!(publish(&tx, "a", "before", &[0.0, 0.0]).is_err());
        assert!(publish(&tx, "a", "before", &[1.0]).is_err());
        assert!(publish(&tx, "a", "before", &[1.0, 0.0]).unwrap());
        assert!(!publish(&tx, "a", "before", &[0.0, 1.0]).unwrap());
        assert!(unfinished_text(&tx, "a", "before").unwrap().is_none());
        assert!(configure(&tx, "before", 1).is_err());
        assert_eq!(rank(&tx, "before", &[1.0, 0.0], 1).unwrap().len(), 1);

        configure(&tx, "after", 3).unwrap();
        assert!(unfinished_text(&tx, "b", "before").unwrap().is_none());
        assert!(!publish(&tx, "b", "before", &[1.0, 0.0]).unwrap());
        assert!(rank(&tx, "after", &[1.0, 0.0, 0.0], 1).unwrap().is_empty());
        assert!(publish(&tx, "a", "after", &[1.0, 0.0, 0.0]).unwrap());
        tx.execute("DELETE FROM sources WHERE source_id='a'", [])
            .unwrap();
        assert!(rank(&tx, "after", &[1.0, 0.0, 0.0], 1).unwrap().is_empty());
        assert!(!publish(&tx, "a", "after", &[1.0, 0.0, 0.0]).unwrap());
        tx.commit().unwrap();
    }

    #[test]
    fn detects_corrupt_vectors_and_bounds_the_scan() {
        assert!(
            validate_scan(
                MAX_SCAN_BYTES / (MAX_DIMENSIONS * FLOAT_BYTES),
                MAX_DIMENSIONS
            )
            .is_ok()
        );
        assert!(
            validate_scan(
                MAX_SCAN_BYTES / (MAX_DIMENSIONS * FLOAT_BYTES) + 1,
                MAX_DIMENSIONS
            )
            .is_err()
        );
        assert!(validate_scan(MAX_SCAN_ROWS + 1, 1).is_err());
        let mut db = index();
        let tx = db.transaction_with_behavior(Immediate).unwrap();
        configure(&tx, "model", 2).unwrap();
        add(&tx, "a");
        assert!(publish(&tx, "a", "model", &[1.0, 0.0]).unwrap());
        let invalid: Vec<_> = [f32::NAN, 1.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        tx.execute("UPDATE chunk_vectors SET embedding=?1", [invalid])
            .unwrap();
        assert!(rank(&tx, "model", &[1.0, 0.0], 1).is_err());
    }
}
