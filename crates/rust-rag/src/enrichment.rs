//! Durable, revision-bound scheduling for optional enrichments.

#[cfg(any(feature = "model", feature = "dashboard"))]
use anyhow::Result;
#[cfg(feature = "model")]
use anyhow::ensure;
#[cfg(any(feature = "model", feature = "dashboard"))]
use rusqlite::{Connection, OptionalExtension, params};
#[cfg(feature = "model")]
use serde_json::{Value, json};

pub(crate) const SCHEMA: &str = "
CREATE TABLE enrichment_attempts (
    lane TEXT NOT NULL CHECK (lane IN ('vector','graph')),
    chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    failures INTEGER NOT NULL DEFAULT 0 CHECK (failures BETWEEN 0 AND 3),
    next_at INTEGER NOT NULL DEFAULT 0,
    lease BLOB,
    PRIMARY KEY(lane,chunk_id,fingerprint)
);
CREATE TABLE enrichment_pauses (
    lane TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    next_at INTEGER NOT NULL,
    PRIMARY KEY(lane,fingerprint)
);
CREATE TRIGGER clear_vector_attempts AFTER DELETE ON vector_config BEGIN
    DELETE FROM enrichment_attempts WHERE lane='vector' AND fingerprint=old.fingerprint;
    DELETE FROM enrichment_pauses WHERE lane='vector' AND fingerprint=old.fingerprint;
END;
CREATE TRIGGER clear_graph_attempts AFTER DELETE ON graph_config BEGIN
    DELETE FROM enrichment_attempts WHERE lane='graph' AND fingerprint=old.fingerprint;
    DELETE FROM enrichment_pauses WHERE lane='graph' AND fingerprint=old.fingerprint;
END;";

#[cfg(feature = "model")]
const NOT_PAUSED: &str = "NOT EXISTS(SELECT 1 FROM enrichment_pauses
                            WHERE lane=?2 AND fingerprint=?1 AND next_at>unixepoch())";

#[derive(Clone, Copy)]
#[cfg(any(feature = "model", feature = "dashboard"))]
pub(crate) enum Lane {
    Vector,
    Graph,
}

#[cfg(any(feature = "model", feature = "dashboard"))]
impl Lane {
    fn name(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Graph => "graph",
        }
    }

    fn unfinished(self) -> String {
        let (config, finished) = match self {
            Self::Vector => ("vector_config", "chunk_vectors"),
            Self::Graph => ("graph_config", "graph_chunks"),
        };
        format!(
            "FROM chunks c JOIN {config} m ON m.id=1 AND m.fingerprint=?1
             LEFT JOIN {finished} f ON f.chunk_row=c.row_id
             LEFT JOIN enrichment_attempts a ON a.chunk_id=c.chunk_id
                 AND a.fingerprint=m.fingerprint AND a.lane=?2
             WHERE f.chunk_row IS NULL"
        )
    }
}

#[cfg(feature = "model")]
pub(crate) fn ready(
    db: &Connection,
    lane: Lane,
    fingerprint: &str,
    limit: usize,
) -> Result<Vec<String>> {
    ensure!((1..=256).contains(&limit), "Choose 1 to 256 passages.");
    let mut select = db.prepare(&format!(
        "SELECT c.chunk_id {} AND coalesce(a.failures,0)<3
         AND coalesce(a.next_at,0)<=unixepoch() AND {NOT_PAUSED}
         ORDER BY coalesce(a.next_at,0),c.row_id LIMIT ?3",
        lane.unfinished()
    ))?;
    let rows = select.query_map(params![fingerprint, lane.name(), limit as i64], |row| {
        row.get(0)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(feature = "model")]
pub(crate) struct Claim {
    lane: Lane,
    chunk_id: String,
    fingerprint: String,
    token: Vec<u8>,
}

#[cfg(feature = "model")]
impl Claim {
    pub(crate) fn current(&self, db: &Connection) -> Result<bool> {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrichment_attempts
             WHERE lane=?1 AND chunk_id=?2 AND fingerprint=?3 AND lease=?4
               AND next_at>unixepoch())",
            params![
                self.lane.name(),
                self.chunk_id,
                self.fingerprint,
                self.token
            ],
            |row| row.get(0),
        )?)
    }
}

#[cfg(feature = "model")]
pub(crate) fn claim(
    db: &Connection,
    lane: Lane,
    fingerprint: &str,
    chunk_id: &str,
) -> Result<Option<Claim>> {
    ensure!(!db.is_autocommit(), "Claim work in a write transaction.");
    let token = db
        .query_row(
            &format!(
                "INSERT INTO enrichment_attempts(lane,chunk_id,fingerprint,next_at,lease)
             SELECT ?2,c.chunk_id,?1,unixepoch()+300,randomblob(16) {}
               AND c.chunk_id=?3 AND coalesce(a.failures,0)<3
               AND coalesce(a.next_at,0)<=unixepoch() AND {NOT_PAUSED}
             ON CONFLICT(lane,chunk_id,fingerprint)
             DO UPDATE SET lease=excluded.lease,next_at=excluded.next_at
             RETURNING lease",
                lane.unfinished()
            ),
            params![fingerprint, lane.name(), chunk_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(token.map(|token| Claim {
        lane,
        chunk_id: chunk_id.to_owned(),
        fingerprint: fingerprint.to_owned(),
        token,
    }))
}

#[cfg(feature = "model")]
pub(crate) fn completed(db: &Connection, claim: &Claim) -> Result<bool> {
    ensure!(!db.is_autocommit(), "Complete work in a write transaction.");
    Ok(db.execute(
        "DELETE FROM enrichment_attempts WHERE lane=?1 AND chunk_id=?2 AND fingerprint=?3
         AND lease=?4 AND next_at>unixepoch()",
        params![
            claim.lane.name(),
            claim.chunk_id,
            claim.fingerprint,
            claim.token
        ],
    )? == 1)
}

#[cfg(feature = "model")]
pub(crate) fn failed(db: &Connection, claim: &Claim) -> Result<bool> {
    reschedule(db, claim, true)
}

#[cfg(feature = "model")]
pub(crate) fn release(db: &Connection, claim: &Claim) -> Result<bool> {
    reschedule(db, claim, false)
}

#[cfg(feature = "model")]
pub(crate) fn pause(db: &Connection, lane: Lane, fingerprint: &str, seconds: u64) -> Result<()> {
    db.execute(
        "INSERT INTO enrichment_pauses VALUES(?1,?2,unixepoch()+?3)
         ON CONFLICT(lane,fingerprint) DO UPDATE SET next_at=max(next_at,excluded.next_at)",
        params![lane.name(), fingerprint, seconds.clamp(1, 300) as i64],
    )?;
    Ok(())
}

#[cfg(any(feature = "model", feature = "dashboard"))]
pub(crate) fn paused_until(db: &Connection, lane: Lane, fingerprint: &str) -> Result<Option<i64>> {
    Ok(db.query_row(
        "SELECT next_at FROM enrichment_pauses WHERE lane=?1 AND fingerprint=?2 AND next_at>unixepoch()",
        params![lane.name(), fingerprint], |row| row.get(0)
    ).optional()?)
}

#[cfg(feature = "model")]
fn reschedule(db: &Connection, claim: &Claim, strike: bool) -> Result<bool> {
    ensure!(
        !db.is_autocommit(),
        "Reschedule work in a write transaction."
    );
    // A transport failure rotates to other passages without attributing a
    // provider outage to the content. Item failures get 2s and 8s delays.
    Ok(db.execute(
        "UPDATE enrichment_attempts
         SET failures=failures+?5,lease=NULL,
             next_at=unixepoch()+CASE WHEN ?5 THEN (2 << (failures*2)) ELSE 5 END
         WHERE lane=?1 AND chunk_id=?2 AND fingerprint=?3 AND lease=?4 AND next_at>unixepoch()",
        params![
            claim.lane.name(),
            claim.chunk_id,
            claim.fingerprint,
            claim.token,
            strike
        ],
    )? == 1)
}

#[cfg(any(feature = "model", feature = "dashboard"))]
#[cfg_attr(feature = "dashboard", derive(serde::Serialize))]
pub(crate) struct ProgressCounts {
    pub(crate) pending: i64,
    pub(crate) ready: i64,
    pub(crate) leased: i64,
    pub(crate) deferred: i64,
    pub(crate) quarantined: i64,
    pub(crate) next_retry_unix: Option<i64>,
    pub(crate) provider_paused_until_unix: Option<i64>,
}

#[cfg(any(feature = "model", feature = "dashboard"))]
pub(crate) fn progress_counts(
    db: &Connection,
    lane: Lane,
    fingerprint: &str,
) -> Result<ProgressCounts> {
    let (pending, ready, leased, deferred, quarantined, retry_at): (i64, i64, i64, i64, i64, Option<i64>) =
        db.query_row(
            &format!(
                "SELECT count(*),
                 count(*) FILTER (WHERE coalesce(a.failures,0)<3 AND coalesce(a.next_at,0)<=unixepoch()),
                 count(*) FILTER (WHERE a.lease IS NOT NULL AND a.next_at>unixepoch()),
                 count(*) FILTER (WHERE a.lease IS NULL AND a.failures<3 AND a.next_at>unixepoch()),
                 count(*) FILTER (WHERE a.failures=3),
                 min(a.next_at) FILTER (WHERE a.failures<3 AND a.next_at>unixepoch())
                 {}",
                lane.unfinished()
            ),
            params![fingerprint, lane.name()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )?;
    let paused = paused_until(db, lane, fingerprint)?;
    Ok(ProgressCounts {
        pending,
        ready: if paused.is_some() { 0 } else { ready },
        leased,
        deferred,
        quarantined,
        next_retry_unix: retry_at,
        provider_paused_until_unix: paused,
    })
}

#[cfg(feature = "model")]
pub(crate) fn progress(db: &Connection, lane: Lane, fingerprint: &str) -> Result<Value> {
    let counts = progress_counts(db, lane, fingerprint)?;
    let mut select = db.prepare(
        "SELECT chunk_id FROM enrichment_attempts
         WHERE lane=?1 AND fingerprint=?2 AND failures=3 ORDER BY chunk_id LIMIT 32",
    )?;
    let ids = select
        .query_map(params![lane.name(), fingerprint], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({
        "pending": counts.pending, "ready": counts.ready,
        "leased": counts.leased, "deferred": counts.deferred,
        "quarantined": counts.quarantined, "quarantined_ids": ids, "next_retry_unix": counts.next_retry_unix,
        "provider_paused_until_unix": counts.provider_paused_until_unix,
    }))
}

#[cfg(feature = "model")]
pub(crate) fn retry(db: &Connection, lane: Lane, fingerprint: &str) -> Result<usize> {
    Ok(db.execute(
        "DELETE FROM enrichment_attempts WHERE lane=?1 AND fingerprint=?2
         AND (lease IS NULL OR next_at<=unixepoch())",
        params![lane.name(), fingerprint],
    )?)
}

#[cfg(all(feature = "model", test))]
mod tests {
    use super::*;

    #[test]
    fn failed_jobs_yield_and_late_claims_cannot_publish() -> Result<()> {
        let state = tempfile::tempdir()?;
        let root = tempfile::tempdir()?;
        for name in ["a", "b", "c"] {
            std::fs::write(
                root.path().join(format!("{name}.md")),
                format!("{name} synthetic passage"),
            )?;
        }
        let mut db = crate::store::open(state.path(), true)?;
        crate::store::upgrade(&mut db, state.path())?;
        crate::store::ingest(
            &mut db,
            &crate::files::directory(root.path())?,
            false,
            false,
            false,
        )?;
        let tx = db.transaction()?;
        crate::vector::configure(&tx, "before", 2)?;
        tx.commit()?;
        let lane = Lane::Vector;
        let ids = ready(&db, lane, "before", 3)?;
        assert_eq!(ids.len(), 3);

        // The low-ID passage times out. On the next invocation both
        // neighboring passages can complete without waiting for the timeout.
        let report = crate::run_enrichment(
            &mut db,
            lane,
            "before",
            1,
            false,
            |tx, id| crate::vector::unfinished_text(tx, id, "before"),
            |_| Err::<Option<Vec<f32>>, _>(crate::model::Failure::Provider.into()),
            |_, _, _| unreachable!(),
        )?;
        assert_eq!(report["provider_status"], "provider_unavailable");
        assert_eq!(report["progress"]["quarantined"], 0);
        drop(db);
        let mut db = crate::store::open(state.path(), false)?;
        let report = crate::run_enrichment(
            &mut db,
            lane,
            "before",
            2,
            false,
            |tx, id| crate::vector::unfinished_text(tx, id, "before"),
            |_| Ok(Some(vec![1.0, 0.0])),
            |tx, id, values| crate::vector::publish(tx, id, "before", &values),
        )?;
        assert_eq!(report["completed_chunks"], 2);
        assert_eq!(report["progress"]["pending"], 1);

        // Advance the bounded clock delays deterministically. Three
        // item-level failures quarantine just that passage.
        for _ in 0..3 {
            db.execute("UPDATE enrichment_attempts SET next_at=0", [])?;
            crate::run_enrichment(
                &mut db,
                lane,
                "before",
                1,
                false,
                |tx, id| crate::vector::unfinished_text(tx, id, "before"),
                |_| Err::<Option<Vec<f32>>, _>(crate::model::Failure::Item.into()),
                |_, _, _| unreachable!(),
            )?;
        }
        assert_eq!(
            progress(&db, lane, "before")?["quarantined_ids"],
            json!([&ids[0]])
        );
        assert!(ready(&db, lane, "before", 3)?.is_empty());
        let tx = db.transaction()?;
        assert_eq!(retry(&tx, lane, "before")?, 1);
        let old = claim(&tx, lane, "before", &ids[0])?.unwrap();
        assert!(claim(&tx, lane, "before", &ids[0])?.is_none());
        tx.commit()?;

        let tx = db.transaction()?;
        crate::vector::configure(&tx, "after", 2)?;
        crate::vector::configure(&tx, "before", 2)?;
        assert!(!old.current(&tx)?);
        pause(&tx, lane, "before", 60)?;
        assert!(ready(&tx, lane, "before", 3)?.is_empty());
        assert_eq!(progress(&tx, lane, "before")?["ready"], 0);
        assert!(claim(&tx, lane, "before", &ids[0])?.is_none());
        tx.commit()?;
        Ok(())
    }
}
