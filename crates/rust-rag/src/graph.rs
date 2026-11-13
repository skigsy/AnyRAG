//! Owner-local graph assertions anchored to active chunks in the evidence index.

use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::HashSet;

#[cfg(any(feature = "model", test))]
use crate::files::digest;
#[cfg(any(feature = "model", test))]
use rusqlite::OptionalExtension;

const MAX_ASSERTIONS: usize = 16;
const MAX_SEEDS: usize = 8;
const MAX_CATALOG_IDS: usize = 4;
const MAX_HOPS: usize = 2;
const MAX_ADDITIONS: usize = 32;
const MAX_CHECKS: usize = 1_000;
pub(crate) const LOCAL_REVIEW_FINGERPRINT_PREFIX: &str = "review-v1:";
const EXTRACTED_UNVERIFIED: &str = "extracted_unverified";
#[cfg(any(feature = "model", test))]
const MAX_BATCH: usize = 256;

pub const SCHEMA: &str = "
CREATE TABLE graph_config (
    id INTEGER PRIMARY KEY CHECK (id=1),
    fingerprint TEXT NOT NULL UNIQUE
);
CREATE TABLE graph_chunks (
    chunk_row INTEGER PRIMARY KEY REFERENCES chunks(row_id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL REFERENCES graph_config(fingerprint) ON DELETE CASCADE
);
CREATE INDEX graph_chunks_by_fingerprint ON graph_chunks(fingerprint);
CREATE TABLE graph_assertions (
    edge_id TEXT PRIMARY KEY NOT NULL,
    chunk_row INTEGER NOT NULL REFERENCES graph_chunks(chunk_row) ON DELETE CASCADE,
    subject TEXT NOT NULL, subject_key TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL, object_key TEXT NOT NULL,
    quote TEXT NOT NULL, quote_start INTEGER NOT NULL, quote_end INTEGER NOT NULL
);
CREATE INDEX graph_assertions_by_chunk ON graph_assertions(chunk_row);
CREATE INDEX graph_assertions_by_subject ON graph_assertions(subject_key,edge_id);
CREATE INDEX graph_assertions_by_object ON graph_assertions(object_key,edge_id);
";

#[cfg(feature = "model")]
pub(crate) fn install_review_queue(db: &Connection) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "Graph review setup requires a write transaction."
    );
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS graph_proposals (
            proposal_id TEXT PRIMARY KEY NOT NULL DEFAULT ('proposal_' || lower(hex(randomblob(16)))),
            edge_id TEXT NOT NULL UNIQUE,
            chunk_row INTEGER NOT NULL REFERENCES graph_chunks(chunk_row) ON DELETE CASCADE,
            subject TEXT NOT NULL, subject_key TEXT NOT NULL, predicate TEXT NOT NULL,
            object TEXT NOT NULL, object_key TEXT NOT NULL,
            quote TEXT NOT NULL, quote_start INTEGER NOT NULL, quote_end INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS graph_proposals_by_chunk ON graph_proposals(chunk_row);",
    )?;
    Ok(())
}

#[cfg(any(feature = "model", feature = "dashboard"))]
fn review_queue_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='graph_proposals')",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(any(feature = "model", feature = "dashboard"))]
pub(crate) fn review_queue(db: &Connection, limit: usize) -> Result<Value> {
    ensure!((1..=100).contains(&limit), "Choose 1 to 100 proposals.");
    let mut proposals = Vec::new();
    if review_queue_exists(db)? {
        let mut statement = db.prepare(
            "SELECT a.proposal_id,a.subject,a.predicate,a.object,a.quote,s.path,
                    c.chunk_id,s.source_hash,a.quote_start,a.quote_end
             FROM graph_proposals a JOIN chunks c ON c.row_id=a.chunk_row
             JOIN sources s USING(source_id)
             ORDER BY a.rowid LIMIT ?1",
        )?;
        proposals = statement
            .query_map([limit as i64 + 1], |row| {
                Ok(json!({
                    "proposal_id": row.get::<_, String>(0)?,
                    "subject": row.get::<_, String>(1)?,
                    "predicate": row.get::<_, String>(2)?,
                    "object": row.get::<_, String>(3)?,
                    "quote": row.get::<_, String>(4)?,
                    "path": row.get::<_, String>(5)?,
                    "chunk_id": row.get::<_, String>(6)?,
                    "revision": row.get::<_, String>(7)?,
                    "start": row.get::<_, i64>(8)?,
                    "end": row.get::<_, i64>(9)?,
                }))
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    let has_more = proposals.len() > limit;
    proposals.truncate(limit);
    Ok(json!({"schema_version": 1, "proposals": proposals, "has_more": has_more}))
}

#[cfg(feature = "model")]
fn valid_proposal_id(id: &str) -> bool {
    id.strip_prefix("proposal_").is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

#[cfg(feature = "model")]
fn insert_approved_assertion(db: &Connection, id: &str) -> Result<()> {
    db.execute(
        "INSERT INTO graph_assertions
         (edge_id,chunk_row,subject,subject_key,predicate,object,object_key,quote,quote_start,quote_end)
         SELECT edge_id,chunk_row,subject,subject_key,predicate,object,object_key,quote,quote_start,quote_end
         FROM graph_proposals WHERE proposal_id=?1",
        [id],
    )?;
    Ok(())
}

#[cfg(feature = "model")]
pub(crate) fn decide_proposal(db: &mut Connection, id: &str, approve: bool) -> Result<Value> {
    use anyhow::Context;

    ensure!(
        valid_proposal_id(id),
        "Use a proposal ID from graph-review list."
    );
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(review_queue_exists(&tx)?, "Proposal is no longer pending.");
    if approve {
        let (root, source) = tx
            .query_row(
                "SELECT st.root,s.path,s.source_hash,c.start,c.end,p.text
                 FROM graph_proposals a JOIN chunks c ON c.row_id=a.chunk_row
                 JOIN passages p ON p.rowid=c.row_id JOIN sources s USING(source_id)
                 JOIN settings st ON st.id=1 WHERE a.proposal_id=?1",
                [id],
                |row| {
                    Ok((
                        std::path::PathBuf::from(row.get::<_, String>(0)?),
                        SourcePassage {
                            path: row.get::<_, String>(1)?.into(),
                            source_hash: row.get(2)?,
                            start: row.get(3)?,
                            end: row.get(4)?,
                            text: row.get(5)?,
                        },
                    ))
                },
            )
            .optional()?
            .context("Proposal is no longer pending.")?;
        crate::files::folder_stamp(&root)?;
        let mut remaining = crate::files::MAX_BATCH_BYTES;
        source
            .read_current(&root, &mut remaining)
            .context("The source changed or is unavailable. Sync the index before reviewing it.")?;
        insert_approved_assertion(&tx, id)?;
    }
    ensure!(
        tx.execute("DELETE FROM graph_proposals WHERE proposal_id=?1", [id])? == 1,
        "Proposal is no longer pending."
    );
    tx.commit()?;
    Ok(json!({
        "schema_version": 1,
        "proposal_id": id,
        "status": if approve { "approved" } else { "dismissed" },
    }))
}

/// Begin enrichment under one extractor configuration in the caller's write transaction.
/// Switching configurations retires old assertions before any new ones become visible.
#[cfg(any(feature = "model", test))]
pub fn configure(db: &Connection, fingerprint: &str) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "Graph configuration requires a write transaction."
    );
    ensure!(
        !fingerprint.is_empty() && fingerprint.len() <= 512,
        "The extractor fingerprint is missing or too long."
    );
    db.execute(
        "DELETE FROM graph_config WHERE fingerprint<>?1",
        [fingerprint],
    )?;
    db.execute(
        "INSERT INTO graph_config(id,fingerprint) VALUES(1,?1) ON CONFLICT(id) DO NOTHING",
        [fingerprint],
    )?;
    Ok(())
}

/// Read a bounded set of unfinished IDs without capturing evidence for later egress.
#[cfg(any(feature = "model", test))]
pub fn pending(db: &Connection, fingerprint: &str, limit: usize) -> Result<Vec<String>> {
    ensure!(
        (1..=MAX_BATCH).contains(&limit),
        "Choose 1 to {MAX_BATCH} passages."
    );
    let mut statement = db.prepare(
        "SELECT c.chunk_id
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN graph_config g ON g.id=1 AND g.fingerprint=?1
         LEFT JOIN graph_chunks a ON a.chunk_row=c.row_id
         WHERE a.chunk_row IS NULL ORDER BY c.row_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![fingerprint, limit as i64], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Resolve the current evidence just before a model call, after earlier calls
/// in the batch may have allowed another process to replace the graph or source.
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
         JOIN graph_config g ON g.id=1 AND g.fingerprint=?2
         LEFT JOIN graph_chunks a ON a.chunk_row=c.row_id
         WHERE c.chunk_id=?1 AND a.chunk_row IS NULL",
            params![chunk_id, fingerprint],
            |row| row.get(0),
        )
        .optional()?)
}

#[cfg(feature = "model")]
fn is_local_text(root: &std::path::Path, path: &std::path::Path) -> bool {
    use std::path::Component;
    path.strip_prefix(root).is_ok_and(|relative| {
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    }) && path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("txt"))
}

#[cfg(feature = "model")]
pub(crate) fn local_text_root(db: &Connection) -> Result<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    let root: Option<String> =
        db.query_row("SELECT root FROM settings WHERE id=1", [], |row| row.get(0))?;
    let root =
        PathBuf::from(root.ok_or_else(|| anyhow::anyhow!("Ingest a local text folder first."))?);
    ensure!(
        root.is_absolute(),
        "The indexed source root must be a local absolute path."
    );
    crate::files::folder_stamp(&root)?;
    let mut statement = db.prepare("SELECT path FROM sources")?;
    for path in statement.query_map([], |row| row.get::<_, String>(0))? {
        let path = path?;
        let path = Path::new(&path);
        ensure!(
            is_local_text(&root, path),
            "Source-clause extraction requires a local Markdown and text index."
        );
    }
    Ok(root)
}

#[cfg(feature = "model")]
pub(crate) struct SourcePassage {
    path: std::path::PathBuf,
    source_hash: String,
    start: i64,
    end: i64,
    pub(crate) text: String,
}

#[cfg(feature = "model")]
impl SourcePassage {
    pub(crate) fn read_current(
        &self,
        root: &std::path::Path,
        remaining: &mut usize,
    ) -> Result<String> {
        use crate::files;
        ensure!(
            is_local_text(root, &self.path),
            "The source is outside the supported local text folder."
        );
        let stamp = files::file_stamp(&self.path)?;
        let raw = files::read_revision(&self.path, remaining, &stamp)?;
        ensure!(
            files::digest(&raw) == self.source_hash,
            "The indexed source revision changed."
        );
        let document = files::decode(&raw)?;
        let start = usize::try_from(self.start)?;
        let end = usize::try_from(self.end)?;
        ensure!(
            start <= end
                && end <= document.chars().count()
                && document
                    .chars()
                    .skip(start)
                    .take(end - start)
                    .eq(self.text.chars()),
            "The indexed passage differs from the source revision."
        );
        Ok(document)
    }
}

#[cfg(feature = "model")]
pub(crate) fn unfinished_source(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
) -> Result<Option<SourcePassage>> {
    Ok(db
        .query_row(
            "SELECT s.path,s.source_hash,c.start,c.end,p.text
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN sources s USING(source_id)
         JOIN graph_config g ON g.id=1 AND g.fingerprint=?2
         LEFT JOIN graph_chunks a ON a.chunk_row=c.row_id
         WHERE c.chunk_id=?1 AND a.chunk_row IS NULL",
            params![chunk_id, fingerprint],
            |row| {
                Ok(SourcePassage {
                    path: row.get::<_, String>(0)?.into(),
                    source_hash: row.get(1)?,
                    start: row.get(2)?,
                    end: row.get(3)?,
                    text: row.get(4)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn entity_key(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn mentions(quote_key: &str, entity: &str) -> bool {
    let word = |character: char| character.is_alphanumeric() || matches!(character, '_' | '-');
    quote_key.match_indices(entity).any(|(start, found)| {
        !quote_key[..start].chars().next_back().is_some_and(word)
            && !quote_key[start + found.len()..]
                .chars()
                .next()
                .is_some_and(word)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectionSupport {
    Forward,
    Reversed,
    Unknown,
}

fn direct_clause(quote: &str, left: &str, verb: &str, right: &str) -> bool {
    let after_left = [quote, quote.strip_prefix("the ").unwrap_or(quote)]
        .into_iter()
        .find_map(|clause| {
            clause
                .strip_prefix(left)
                .and_then(|rest| rest.strip_prefix(' '))
        });
    after_left
        .and_then(|rest| rest.strip_prefix(verb))
        .and_then(|rest| rest.strip_prefix(' '))
        .is_some_and(|end| end == right || end.strip_prefix("the ") == Some(right))
}

fn direction_support(
    predicate: &str,
    subject: &str,
    object: &str,
    quote: &str,
    before: &str,
    after: &str,
) -> DirectionSupport {
    // A quoted substring of a larger clause could be negated, conditional,
    // or attributed to someone else. Only correct a complete direct clause.
    let before_trimmed = before.trim_end_matches([' ', '\t']);
    let separated_start =
        before_trimmed.len() < before.len() || quote.starts_with(char::is_whitespace);
    let starts_clause = match before_trimmed.chars().next_back() {
        None | Some('\r' | '\n') => true,
        Some('.' | '!' | '?') => separated_start,
        _ => false,
    };
    let quote_trimmed = quote.trim_end();
    let boundary_after_period = |rest: &str| rest.chars().next().is_none_or(char::is_whitespace);
    let after_trimmed = after.trim_start_matches([' ', '\t']);
    let ends_clause = if quote_trimmed.ends_with('.') {
        quote_trimmed.len() < quote.len() || boundary_after_period(after)
    } else if let Some(rest) = after_trimmed.strip_prefix('.') {
        boundary_after_period(rest)
    } else {
        after_trimmed
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '\r' | '\n'))
    };
    if !starts_clause || !ends_clause {
        return DirectionSupport::Unknown;
    }
    let (direct, inverse): (&[&str], &[&str]) = match predicate {
        "uses" => (&["uses"], &["is used by"]),
        "requires" => (&["requires"], &["is required by"]),
        "depends_on" => (&["depends on"], &[]),
        "owned_by" => (&["is owned by", "belongs to"], &["owns"]),
        "part_of" => (&["is part of", "is a component of"], &["includes"]),
        _ => return DirectionSupport::Unknown,
    };
    let quote = entity_key(quote);
    let clause = quote.strip_suffix('.').unwrap_or(&quote);
    let supports = |left, right| {
        direct
            .iter()
            .any(|verb| direct_clause(clause, left, verb, right))
            || inverse
                .iter()
                .any(|verb| direct_clause(clause, right, verb, left))
    };
    match (supports(subject, object), supports(object, subject)) {
        (true, false) => DirectionSupport::Forward,
        (false, true) => DirectionSupport::Reversed,
        _ => DirectionSupport::Unknown,
    }
}

fn catalog_id(entity: &str) -> bool {
    let mut chars = entity.chars();
    chars.next().is_some_and(char::is_alphanumeric)
        && chars.next_back().is_some_and(char::is_alphanumeric)
        && (3..=48).contains(&entity.chars().count())
        && entity
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
        && entity.chars().any(char::is_alphabetic)
        && entity.chars().any(|c| c.is_ascii_digit())
}

fn precise_mention(question: &str, entity: &str) -> bool {
    let name_words = entity
        .split_whitespace()
        .filter(|word| {
            !matches!(*word, "a" | "an" | "the") && word.chars().any(char::is_alphanumeric)
        })
        .take(2)
        .count();
    (name_words == 2 || catalog_id(entity)) && mentions(question, entity)
}

#[cfg(any(feature = "model", test))]
struct Assertion<'a> {
    subject: &'a str,
    subject_key: String,
    predicate: &'a str,
    object: &'a str,
    object_key: String,
    quote: &'a str,
    start: i64,
    end: i64,
}

#[cfg(any(feature = "model", test))]
fn validate<'a>(
    value: &'a Value,
    text: &str,
    chunk_start: i64,
    predicates: &[String],
) -> Result<Assertion<'a>> {
    let invalid_fields =
        || anyhow::anyhow!("A graph assertion needs subject, predicate, object and quote strings.");
    let fields = value
        .as_object()
        .filter(|fields| fields.len() == 4)
        .ok_or_else(invalid_fields)?;
    let get = |name| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(invalid_fields)
    };
    let mut subject = get("subject")?.trim();
    let predicate = get("predicate")?;
    let mut object = get("object")?.trim();
    let quote = get("quote")?;
    ensure!(
        [subject, object].iter().all(|value| {
            !value.is_empty()
                && value.chars().count() <= 160
                && !value.chars().any(char::is_control)
        }) && predicates.iter().any(|allowed| allowed == predicate),
        "A graph assertion exceeds the entity bounds or allowed vocabulary."
    );
    ensure!(
        !quote.trim().is_empty(),
        "A graph assertion needs a supporting quote."
    );
    let position = text.find(quote).ok_or_else(|| {
        anyhow::anyhow!(
            "A graph assertion needs an exact supporting quote mentioning both entities."
        )
    })?;
    let after_first = position + quote.chars().next().expect("nonempty quote").len_utf8();
    let mut subject_key = entity_key(subject);
    let mut object_key = entity_key(object);
    let quote_key = entity_key(quote);
    ensure!(
        subject_key != object_key
            && !text[after_first..].contains(quote)
            && mentions(&quote_key, &subject_key)
            && mentions(&quote_key, &object_key),
        "A graph assertion needs one exact supporting quote mentioning two distinct entities."
    );
    if direction_support(
        predicate,
        &subject_key,
        &object_key,
        quote,
        &text[..position],
        &text[position + quote.len()..],
    ) == DirectionSupport::Reversed
    {
        std::mem::swap(&mut subject, &mut object);
        std::mem::swap(&mut subject_key, &mut object_key);
    }
    let start = chunk_start + text[..position].chars().count() as i64;
    let end = start + quote.chars().count() as i64;
    Ok(Assertion {
        subject,
        subject_key,
        predicate,
        object,
        object_key,
        quote,
        start,
        end,
    })
}

/// Re-resolve the source and extractor after the external model call. The caller commits
/// only on success. A changed chunk or configuration causes a no-op.
#[cfg(any(feature = "model", test))]
pub fn publish(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
    predicates: &[String],
    assertions: &[Value],
) -> Result<bool> {
    write_assertions(
        db,
        chunk_id,
        fingerprint,
        predicates,
        assertions,
        "graph_assertions",
    )
}

#[cfg(feature = "model")]
pub(crate) fn stage(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
    predicates: &[String],
    assertions: &[Value],
) -> Result<bool> {
    write_assertions(
        db,
        chunk_id,
        fingerprint,
        predicates,
        assertions,
        "graph_proposals",
    )
}

#[cfg(any(feature = "model", test))]
fn write_assertions(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
    predicates: &[String],
    assertions: &[Value],
    table: &str,
) -> Result<bool> {
    ensure!(
        !db.is_autocommit(),
        "Graph publication requires a write transaction."
    );
    let Some((row_id, text, start)) = publication_chunk(db, chunk_id, fingerprint)? else {
        return Ok(false);
    };
    ensure!(
        assertions.len() <= MAX_ASSERTIONS,
        "The extractor returned more than {MAX_ASSERTIONS} assertions for a passage."
    );
    let validated = assertions
        .iter()
        .map(|value| validate(value, &text, start, predicates))
        .collect::<Result<Vec<_>>>()?;
    db.execute(
        "INSERT INTO graph_chunks VALUES(?1,?2)",
        params![row_id, fingerprint],
    )?;
    let mut insert = assertion_insert(db, table)?;
    for assertion in validated {
        let id = assertion_id(chunk_id, &assertion);
        insert_assertion(&mut insert, &id, row_id, &assertion)?;
    }
    Ok(true)
}

#[cfg(any(feature = "model", test))]
fn publication_chunk(
    db: &Connection,
    chunk_id: &str,
    fingerprint: &str,
) -> Result<Option<(i64, String, i64)>> {
    Ok(db
        .query_row(
            "SELECT c.row_id,p.text,c.start
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         JOIN graph_config g ON g.id=1 AND g.fingerprint=?2
         LEFT JOIN graph_chunks a ON a.chunk_row=c.row_id
         WHERE c.chunk_id=?1 AND a.chunk_row IS NULL",
            params![chunk_id, fingerprint],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?)
}

#[cfg(any(feature = "model", test))]
fn assertion_insert<'a>(db: &'a Connection, table: &str) -> Result<rusqlite::Statement<'a>> {
    Ok(db.prepare(&format!(
        "INSERT INTO {table}
         (edge_id,chunk_row,subject,subject_key,predicate,object,object_key,quote,quote_start,quote_end)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(edge_id) DO NOTHING"
    ))?)
}

#[cfg(any(feature = "model", test))]
fn assertion_id(chunk_id: &str, assertion: &Assertion<'_>) -> String {
    let key = json!([
        chunk_id,
        assertion.subject_key,
        assertion.predicate,
        assertion.object_key,
        assertion.start,
        assertion.end
    ]);
    format!("edge_{}", digest(key.to_string().as_bytes()))
}

#[cfg(any(feature = "model", test))]
fn insert_assertion(
    insert: &mut rusqlite::Statement<'_>,
    id: &str,
    row_id: i64,
    assertion: &Assertion<'_>,
) -> Result<usize> {
    Ok(insert.execute(params![
        id,
        row_id,
        assertion.subject,
        assertion.subject_key,
        assertion.predicate,
        assertion.object,
        assertion.object_key,
        assertion.quote,
        assertion.start,
        assertion.end
    ])?)
}

#[cfg(feature = "shared")]
pub(crate) use shared_publication::{
    SharedDecision, SharedDisposition, SharedItem, SharedReviewError, decide_shared,
    install_shared_review, publish_shared, shared_review_queue,
};

#[cfg(feature = "shared")]
mod shared_publication {
    use super::*;
    use std::{collections::HashMap, fmt, path::Path};

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum SharedDisposition {
        Automatic { rule: String },
        Review { reason: &'static str },
    }

    pub(crate) struct SharedItem {
        pub value: Value,
        pub disposition: SharedDisposition,
    }

    pub(crate) struct SharedDecision<'a> {
        pub id: &'a str,
        pub approve: bool,
        pub revision: &'a str,
        pub fingerprint: &'a str,
        pub policy_version: i64,
        pub actor: &'a str,
        pub idempotency_key: &'a str,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum SharedReviewError {
        Invalid,
        Stale,
        Conflict,
    }

    impl fmt::Display for SharedReviewError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Invalid => "Use a valid proposal ID, revision, fingerprint, policy version and idempotency key.",
                Self::Stale => "The proposal or source revision changed. Sync the collection and refresh the review queue.",
                Self::Conflict => "The proposal or idempotency key already has a different decision.",
            })
        }
    }

    impl std::error::Error for SharedReviewError {}

    pub(crate) fn install_shared_review(db: &Connection) -> Result<()> {
        install_review_queue(db)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS graph_shared_pending (
                proposal_id TEXT PRIMARY KEY NOT NULL REFERENCES graph_proposals(proposal_id) ON DELETE CASCADE,
                reason TEXT NOT NULL,
                policy_version INTEGER NOT NULL CHECK (policy_version > 0)
            );
            CREATE TABLE IF NOT EXISTS graph_edge_provenance (
                edge_id TEXT PRIMARY KEY NOT NULL REFERENCES graph_assertions(edge_id) ON DELETE CASCADE,
                policy_version INTEGER NOT NULL CHECK (policy_version > 0),
                rule TEXT, reviewer TEXT,
                CHECK ((rule IS NULL) <> (reviewer IS NULL))
            );
            CREATE TABLE IF NOT EXISTS graph_shared_decisions (
                actor TEXT NOT NULL,
                idempotency_key TEXT NOT NULL,
                proposal_id TEXT NOT NULL UNIQUE,
                chunk_row INTEGER NOT NULL REFERENCES graph_chunks(chunk_row) ON DELETE CASCADE,
                revision TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                policy_version INTEGER NOT NULL CHECK (policy_version > 0),
                approve INTEGER NOT NULL CHECK (approve IN (0,1)),
                PRIMARY KEY (actor,idempotency_key)
            );
            CREATE INDEX IF NOT EXISTS graph_shared_decisions_by_chunk ON graph_shared_decisions(chunk_row);",
        )?;
        Ok(())
    }

    fn installed(db: &Connection) -> Result<bool> {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='graph_shared_pending')",
            [],
            |row| row.get(0),
        )?)
    }

    fn token(value: &str, max: usize) -> bool {
        !value.trim().is_empty()
            && value.trim() == value
            && value.len() <= max
            && !value.chars().any(char::is_control)
    }

    pub(crate) fn publish_shared(
        db: &Connection,
        chunk_id: &str,
        fingerprint: &str,
        predicates: &[String],
        items: &[SharedItem],
        policy_version: i64,
    ) -> Result<bool> {
        ensure!(
            !db.is_autocommit(),
            "Graph publication requires a write transaction."
        );
        let Some((row_id, text, start)) = publication_chunk(db, chunk_id, fingerprint)? else {
            return Ok(false);
        };
        let automatic = items
            .iter()
            .filter(|item| matches!(item.disposition, SharedDisposition::Automatic { .. }))
            .count();
        ensure!(
            automatic <= MAX_ASSERTIONS && items.len() - automatic <= MAX_ASSERTIONS,
            "The shared extractor exceeded the per-passage publication limit."
        );
        ensure!(policy_version > 0, SharedReviewError::Invalid);
        let mut seen = HashMap::new();
        let mut validated = Vec::with_capacity(items.len());
        for item in items {
            let assertion = validate(&item.value, &text, start, predicates)?;
            let id = assertion_id(chunk_id, &assertion);
            let label = match &item.disposition {
                SharedDisposition::Automatic { rule } => rule.as_str(),
                SharedDisposition::Review { reason } => reason,
            };
            ensure!(token(label, 128), SharedReviewError::Invalid);
            if let Some(previous) = seen.insert(id.clone(), &item.disposition) {
                ensure!(previous == &item.disposition, SharedReviewError::Conflict);
                continue;
            }
            validated.push((id, assertion, &item.disposition));
        }
        db.execute(
            "INSERT INTO graph_chunks VALUES(?1,?2)",
            params![row_id, fingerprint],
        )?;
        let mut published = assertion_insert(db, "graph_assertions")?;
        let mut pending = assertion_insert(db, "graph_proposals")?;
        for (id, assertion, disposition) in validated {
            match disposition {
                SharedDisposition::Automatic { rule } => {
                    insert_assertion(&mut published, &id, row_id, &assertion)?;
                    db.execute(
                        "INSERT INTO graph_edge_provenance(edge_id,policy_version,rule) VALUES(?1,?2,?3)",
                        params![id, policy_version, rule],
                    )?;
                }
                SharedDisposition::Review { reason } => {
                    insert_assertion(&mut pending, &id, row_id, &assertion)?;
                    db.execute(
                        "INSERT INTO graph_shared_pending(proposal_id,reason,policy_version)
                         SELECT proposal_id,?2,?3 FROM graph_proposals WHERE edge_id=?1",
                        params![id, reason, policy_version],
                    )?;
                }
            }
        }
        Ok(true)
    }

    pub(crate) fn shared_review_queue(
        db: &Connection,
        fingerprint: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            (1..=100).contains(&limit)
                && token(fingerprint, 512)
                && after.is_none_or(valid_proposal_id),
            SharedReviewError::Invalid
        );
        let mut proposals = Vec::new();
        if installed(db)? {
            let mut statement = db.prepare(
                "SELECT p.proposal_id,p.subject,p.predicate,p.object,p.quote,s.path,
                        c.chunk_id,s.source_hash,p.quote_start,p.quote_end,m.reason,m.policy_version
                 FROM graph_shared_pending m JOIN graph_proposals p USING(proposal_id)
                 JOIN graph_chunks g ON g.chunk_row=p.chunk_row
                 JOIN graph_config f ON f.id=1 AND f.fingerprint=g.fingerprint
                 JOIN chunks c ON c.row_id=g.chunk_row JOIN sources s USING(source_id)
                 WHERE g.fingerprint=?1 AND m.proposal_id>?2 ORDER BY m.proposal_id LIMIT ?3",
            )?;
            proposals = statement
                .query_map(
                    params![fingerprint, after.unwrap_or(""), limit as i64 + 1],
                    |row| {
                        Ok(json!({
                            "proposal_id": row.get::<_, String>(0)?,
                            "subject": row.get::<_, String>(1)?,
                            "predicate": row.get::<_, String>(2)?,
                            "object": row.get::<_, String>(3)?,
                            "quote": row.get::<_, String>(4)?,
                            "path": row.get::<_, String>(5)?,
                            "chunk_id": row.get::<_, String>(6)?,
                            "revision": row.get::<_, String>(7)?,
                            "start": row.get::<_, i64>(8)?,
                            "end": row.get::<_, i64>(9)?,
                            "reason": row.get::<_, String>(10)?,
                            "policy_version": row.get::<_, i64>(11)?,
                            "fingerprint": fingerprint,
                        }))
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
        }
        let has_more = proposals.len() > limit;
        proposals.truncate(limit);
        let next_cursor = if has_more {
            proposals.last().map(|row| &row["proposal_id"])
        } else {
            None
        };
        Ok(
            json!({"schema_version":1,"proposals":proposals,"has_more":has_more,"next_cursor":next_cursor}),
        )
    }

    fn current_source(
        db: &Connection,
        chunk: i64,
        root: &Path,
        decision: &SharedDecision<'_>,
    ) -> Result<()> {
        use crate::files;
        use std::path::Component;

        let path = db
            .query_row(
                "SELECT s.path
             FROM graph_chunks g JOIN graph_config f ON f.id=1 AND f.fingerprint=g.fingerprint
             JOIN chunks c ON c.row_id=g.chunk_row JOIN sources s USING(source_id)
             WHERE g.chunk_row=?1 AND g.fingerprint=?2 AND s.source_hash=?3",
                params![chunk, decision.fingerprint, decision.revision],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(SharedReviewError::Stale)?;
        if decision.approve {
            files::folder_stamp(root).map_err(|_| SharedReviewError::Stale)?;
            let indexed_root = files::display_path(root).map_err(|_| SharedReviewError::Stale)?;
            let path = Path::new(&path);
            ensure!(
                path.strip_prefix(indexed_root).is_ok_and(|relative| {
                    !relative.as_os_str().is_empty()
                        && relative
                            .components()
                            .all(|part| matches!(part, Component::Normal(_)))
                }),
                SharedReviewError::Stale
            );
            let stamp = files::file_stamp(path).map_err(|_| SharedReviewError::Stale)?;
            let mut remaining = files::MAX_BATCH_BYTES;
            let raw = files::read_revision(path, &mut remaining, &stamp)
                .map_err(|_| SharedReviewError::Stale)?;
            ensure!(
                files::digest(&raw) == decision.revision,
                SharedReviewError::Stale
            );
        }
        Ok(())
    }

    pub(crate) fn decide_shared(
        db: &Connection,
        root: &Path,
        decision: &SharedDecision<'_>,
    ) -> Result<Value> {
        ensure!(
            !db.is_autocommit(),
            "Graph review requires a write transaction."
        );
        let SharedDecision {
            id,
            approve,
            revision,
            fingerprint,
            policy_version,
            actor,
            idempotency_key,
        } = *decision;
        ensure!(
            valid_proposal_id(id)
                && token(revision, 512)
                && token(fingerprint, 512)
                && policy_version > 0
                && token(actor, 512)
                && token(idempotency_key, 128),
            SharedReviewError::Invalid
        );
        ensure!(installed(db)?, SharedReviewError::Stale);
        let result = || {
            json!({
                "schema_version":1,"proposal_id":id,"status":if approve {"approved"} else {"dismissed"},
                "revision":revision,"fingerprint":fingerprint,"policy_version":policy_version,
            })
        };
        let previous = db
            .query_row(
                "SELECT proposal_id,chunk_row,revision,fingerprint,policy_version,approve
             FROM graph_shared_decisions WHERE actor=?1 AND idempotency_key=?2",
                params![actor, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, bool>(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((
            prior_id,
            chunk,
            prior_revision,
            prior_fingerprint,
            prior_policy,
            prior_approve,
        )) = previous
        {
            ensure!(
                (
                    prior_id.as_str(),
                    prior_revision.as_str(),
                    prior_fingerprint.as_str(),
                    prior_policy,
                    prior_approve
                ) == (id, revision, fingerprint, policy_version, approve),
                SharedReviewError::Conflict
            );
            current_source(db, chunk, root, decision)?;
            return Ok(result());
        }
        let decided: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM graph_shared_decisions WHERE proposal_id=?1)",
            [id],
            |row| row.get(0),
        )?;
        ensure!(!decided, SharedReviewError::Conflict);
        let chunk = db.query_row(
            "SELECT p.chunk_row FROM graph_shared_pending m JOIN graph_proposals p USING(proposal_id)
             WHERE p.proposal_id=?1",
            [id], |row| row.get(0),
        ).optional()?.ok_or(SharedReviewError::Stale)?;
        current_source(db, chunk, root, decision)?;
        if approve {
            insert_approved_assertion(db, id)?;
            db.execute(
                "INSERT INTO graph_edge_provenance(edge_id,policy_version,reviewer)
                 SELECT edge_id,?2,?3 FROM graph_proposals WHERE proposal_id=?1",
                params![id, policy_version, actor],
            )?;
        }
        db.execute(
            "INSERT INTO graph_shared_decisions
             (actor,idempotency_key,proposal_id,chunk_row,revision,fingerprint,policy_version,approve)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![actor, idempotency_key, id, chunk, revision, fingerprint, policy_version, approve],
        )?;
        ensure!(
            db.execute("DELETE FROM graph_proposals WHERE proposal_id=?1", [id])? == 1,
            SharedReviewError::Stale
        );
        Ok(result())
    }
}

struct Frontier {
    entity: String,
    seed: String,
    chain: Vec<String>,
    score: f64,
    contains_unverified: bool,
}

struct Edge {
    id: String,
    subject_key: String,
    object_key: String,
    chunk: String,
    assertion: Value,
    assertion_status: String,
    evidence: Value,
}

struct ProvenanceSql {
    join: &'static str,
    status: &'static str,
}

impl ProvenanceSql {
    fn read(db: &Connection) -> Result<Self> {
        let (local_review, shared_provenance): (bool, bool) = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM graph_config WHERE fingerprint GLOB ?1),
                    EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='graph_edge_provenance')",
            [format!("{LOCAL_REVIEW_FINGERPRINT_PREFIX}*")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(if local_review {
            // The owner-local review fingerprint only receives assertions through approval.
            Self {
                join: "",
                status: "'reviewed'",
            }
        } else if shared_provenance {
            Self {
                join: "LEFT JOIN graph_edge_provenance gp ON gp.edge_id=a.edge_id",
                status: "CASE WHEN gp.rule IS NOT NULL AND gp.reviewer IS NULL THEN 'declared'
                              WHEN gp.rule IS NULL AND gp.reviewer IS NOT NULL THEN 'reviewed'
                              ELSE 'extracted_unverified' END",
            }
        } else {
            Self {
                join: "",
                status: "'extracted_unverified'",
            }
        })
    }
}

fn seed_entities(
    db: &Connection,
    chunk: &str,
    question: &str,
    provenance: &ProvenanceSql,
) -> Result<(Vec<(String, bool)>, bool)> {
    let ProvenanceSql { join, status } = provenance;
    let mut statement = db.prepare(&format!(
        "SELECT a.subject_key,a.object_key,{status}
         FROM chunks c JOIN graph_assertions a ON a.chunk_row=c.row_id
         {join}
         WHERE c.chunk_id=?1 ORDER BY a.edge_id LIMIT ?2",
    ))?;
    let pairs = statement.query_map(params![chunk, MAX_ASSERTIONS as i64], |row| {
        Ok((
            [row.get::<_, String>(0)?, row.get::<_, String>(1)?],
            row.get::<_, String>(2)? == EXTRACTED_UNVERIFIED,
        ))
    })?;
    let pairs = pairs.collect::<rusqlite::Result<Vec<_>>>()?;
    let exact = pairs
        .iter()
        .any(|(pair, _)| pair.iter().any(|entity| precise_mention(question, entity)));
    let mut entities = std::collections::BTreeMap::new();
    for (pair, unverified) in pairs {
        if !exact || pair.iter().any(|entity| precise_mention(question, entity)) {
            for entity in pair {
                *entities.entry(entity).or_insert(false) |= unverified;
            }
        }
    }
    let mut entities: Vec<_> = entities.into_iter().collect();
    if exact {
        // Explore the other end of the matched assertion first. A direct
        // neighbor of that entity supplies the next source in a two-edge path.
        entities.sort_by_key(|(entity, _)| precise_mention(question, entity));
    }
    Ok((entities, exact))
}

pub type NeighborLookup<'a> = dyn Fn(&str, usize) -> Result<Vec<String>> + 'a;

fn neighbors(
    db: &Connection,
    entity: &str,
    limit: usize,
    lookup: Option<&NeighborLookup<'_>>,
    provenance: &ProvenanceSql,
) -> Result<Vec<Edge>> {
    // Each side is a limited index range scan before merging, so one very
    // common entity cannot make SQLite sort the entire matching edge set.
    let local_ids = "SELECT edge_id FROM (
                 SELECT edge_id FROM graph_assertions WHERE subject_key=?1 ORDER BY edge_id LIMIT ?2
             )
             UNION
             SELECT edge_id FROM (
                 SELECT edge_id FROM graph_assertions WHERE object_key=?1 ORDER BY edge_id LIMIT ?2
             )";
    let (source_join, revision) = crate::store::source_revision_sql(db)?;
    let anchor = crate::store::anchor_sql(db)?;
    let ocr = crate::store::ocr_sql(db)?;
    let ProvenanceSql { join, status } = provenance;
    let mut candidate_count = None;
    let mut encoded = None;
    let candidates = if let Some(lookup) = lookup {
        let ids = lookup(entity, limit)?;
        ensure!(ids.len() <= limit, "Graph candidate limit exceeded.");
        candidate_count = Some(ids.len());
        encoded = Some(serde_json::to_string(&ids)?);
        "SELECT value AS edge_id FROM json_each(?3)"
    } else {
        local_ids
    };
    let mut statement = db.prepare(&format!(
        "WITH ids AS ({candidates})
         SELECT a.edge_id,a.subject,a.subject_key,a.predicate,a.object,a.object_key,
                a.quote,a.quote_start,a.quote_end,c.chunk_id,c.start,c.end,c.page,
                s.source_id,s.path,s.source_hash,p.text,{revision},{anchor},{ocr},{status}
         FROM ids JOIN graph_assertions a USING(edge_id)
         JOIN chunks c ON c.row_id=a.chunk_row
         JOIN sources s USING(source_id) JOIN passages p ON p.rowid=c.row_id
         {source_join}
         {join}
         WHERE a.subject_key=?1 OR a.object_key=?1
         ORDER BY a.edge_id LIMIT ?2"
    ))?;
    let parameters = params![entity, limit as i64, encoded];
    let rows = statement.query_map(
        if lookup.is_some() { parameters } else { &parameters[..2] },
        |row| {
        let id: String = row.get(0)?;
        let chunk: String = row.get(9)?;
        let hash: String = row.get(15)?;
        let mut evidence = json!({
            "chunk_id": chunk, "source_id": row.get::<_, String>(13)?, "path": row.get::<_, String>(14)?,
            "source_hash": hash, "revision": row.get::<_, String>(17)?, "generation": hash,
            "start": row.get::<_, i64>(10)?, "end": row.get::<_, i64>(11)?,
            "page": row.get::<_, Option<i64>>(12)?, "text": row.get::<_, String>(16)?,
            "anchor": row.get::<_, Option<String>>(18)?,
            "evidence_role": "candidate_context", "answer_support": "unverified",
            "match_status": "weak_match",
            "ranking": {"method": "graph_assertion", "lexical_score": null, "cosine_similarity": null},
        });
        crate::store::attach_ocr(&mut evidence, row, 19)?;
        Ok(Edge {
            id: id.clone(), subject_key: row.get(2)?, object_key: row.get(5)?, chunk: chunk.clone(),
            assertion_status: row.get(20)?,
            assertion: json!({
                "edge_id": id, "subject": row.get::<_, String>(1)?, "predicate": row.get::<_, String>(3)?,
                "object": row.get::<_, String>(4)?, "quote": row.get::<_, String>(6)?,
                "start": row.get::<_, i64>(7)?, "end": row.get::<_, i64>(8)?
            }),
            evidence,
        })
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        candidate_count.is_none_or(|count| count == rows.len()),
        "The graph returned a candidate outside the active evidence snapshot."
    );
    Ok(rows)
}

fn catalog_anchor(
    db: &Connection,
    question: &str,
    lookup: Option<&NeighborLookup<'_>>,
    provenance: &ProvenanceSql,
) -> Result<(Option<(String, Edge)>, usize)> {
    let mut seen = HashSet::new();
    let mut lookups = 0;
    for id in question
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_')))
        .filter(|id| catalog_id(id) && seen.insert(*id))
        .take(MAX_CATALOG_IDS)
    {
        lookups += 1;
        if let Some(edge) = neighbors(db, id, 1, lookup, provenance)?.pop() {
            return Ok((Some((id.into(), edge)), lookups));
        }
    }
    Ok((None, lookups))
}

fn graph_hit(
    mut edge: Edge,
    node: &Frontier,
    hop: usize,
    chain: &[String],
    seed_match: &str,
) -> Value {
    let direction = assertion_direction(&edge.evidence, &edge.assertion);
    edge.evidence["score"] = json!(node.score / (hop + 1) as f64);
    edge.evidence["graph_support"] = json!({
        "seed_chunk_id": node.seed, "hop": hop, "supporting_chunk_ids": chain,
        "assertion": edge.assertion, "assertion_status": edge.assertion_status, "seed_match": seed_match,
        "path_contains_unverified_assertion": node.contains_unverified || edge.assertion_status == EXTRACTED_UNVERIFIED,
        "direction_support": match direction {
            DirectionSupport::Forward => "source_clause",
            DirectionSupport::Reversed => "contradicted_by_source_clause",
            DirectionSupport::Unknown => "unverified",
        },
    });
    if direction == DirectionSupport::Reversed {
        let stored = &edge.evidence["graph_support"]["assertion"];
        let source_direction = json!({"subject": stored["object"], "object": stored["subject"]});
        edge.evidence["graph_support"]["source_direction"] = source_direction;
    }
    edge.evidence
}

fn assertion_direction(evidence: &Value, assertion: &Value) -> DirectionSupport {
    let Some(text) = evidence["text"].as_str() else {
        return DirectionSupport::Unknown;
    };
    let Some(quote) = assertion["quote"].as_str() else {
        return DirectionSupport::Unknown;
    };
    let (Some(predicate), Some(subject), Some(object)) = (
        assertion["predicate"].as_str(),
        assertion["subject"].as_str(),
        assertion["object"].as_str(),
    ) else {
        return DirectionSupport::Unknown;
    };
    let Some(offset) = assertion["start"]
        .as_i64()
        .zip(evidence["start"].as_i64())
        .and_then(|(start, base)| start.checked_sub(base))
        .and_then(|start| usize::try_from(start).ok())
    else {
        return DirectionSupport::Unknown;
    };
    let Some((position, _)) = text.char_indices().nth(offset) else {
        return DirectionSupport::Unknown;
    };
    let Some(after) = text[position..].strip_prefix(quote) else {
        return DirectionSupport::Unknown;
    };
    direction_support(
        predicate,
        &entity_key(subject),
        &entity_key(object),
        quote,
        &text[..position],
        after,
    )
}

/// Expand from active assertions inside the same read snapshot as seed retrieval.
pub fn expand(
    db: &Connection,
    seeds: &[Value],
    question: &str,
    limit: usize,
) -> Result<(Vec<Value>, Value)> {
    expand_using(db, seeds, question, limit, None)
}

pub fn expand_external(
    db: &Connection,
    seeds: &[Value],
    question: &str,
    limit: usize,
    lookup: &NeighborLookup<'_>,
) -> Result<(Vec<Value>, Value)> {
    expand_using(db, seeds, question, limit, Some(lookup))
}

fn expand_using(
    db: &Connection,
    seeds: &[Value],
    question: &str,
    limit: usize,
    lookup: Option<&NeighborLookup<'_>>,
) -> Result<(Vec<Value>, Value)> {
    ensure!(
        !db.is_autocommit(),
        "Graph expansion requires the retrieval snapshot."
    );
    ensure!(
        limit <= MAX_ADDITIONS,
        "Choose at most {MAX_ADDITIONS} graph additions."
    );
    let (configured, unfinished): (bool, bool) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM graph_config),
                (SELECT count(*) FROM chunks) != (SELECT count(*) FROM graph_chunks)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut coverage = json!({
        "method": "typed_assertions", "backend": if lookup.is_some() { "neptune" } else { "sqlite" },
        "max_seeds": MAX_SEEDS, "max_hops": MAX_HOPS, "max_additions": limit,
        "checks": 0, "partial": unfinished || !configured || limit == 0, "unavailable": !configured,
        "construction_budget_exhausted": false, "traversal_budget_exhausted": false,
        "result_limit_reached": limit == 0, "total_check_budget": MAX_CHECKS,
        "index_pending": unfinished, "seed_match": null, "catalog_lookups": 0,
        "seed_status": if !configured { "graph_not_configured" } else if limit == 0 {
            "result_limit"
        } else {
            "no_strong_seed"
        },
    });
    if !configured || limit == 0 {
        return Ok((Vec::new(), coverage));
    }
    let provenance = ProvenanceSql::read(db)?;
    let mut visited: HashSet<String> = seeds
        .iter()
        .filter_map(|seed| seed["chunk_id"].as_str().map(str::to_owned))
        .collect();
    let mut traversed = HashSet::new();
    let mut queried = HashSet::new();
    let question = entity_key(question);
    let mut exact_frontier = Vec::new();
    let mut lexical_frontier = Vec::new();
    let mut lexical_seeds = 0;
    for (rank, seed) in seeds.iter().enumerate() {
        let allow_lexical = seed["content_coverage"]["passes"] == true && lexical_seeds < MAX_SEEDS;
        let allow_exact = rank < MAX_SEEDS;
        if !allow_lexical && !allow_exact {
            continue;
        }
        let (Some(chunk), Some(score)) = (seed["chunk_id"].as_str(), seed["score"].as_f64()) else {
            continue;
        };
        let (entities, exact) = seed_entities(
            db,
            chunk,
            if allow_exact { &question } else { "" },
            &provenance,
        )?;
        let frontier = if exact {
            &mut exact_frontier
        } else if allow_lexical {
            lexical_seeds += 1;
            coverage["seed_status"] = json!("no_seed_assertions");
            &mut lexical_frontier
        } else {
            continue;
        };
        for (entity, contains_unverified) in entities {
            frontier.push(Frontier {
                entity,
                seed: chunk.to_owned(),
                chain: vec![chunk.to_owned()],
                score,
                contains_unverified,
            });
        }
    }
    // A contextual catalog ID must not displace a ranked exact seed.
    let (catalog, lookups) = if exact_frontier.is_empty() {
        match catalog_anchor(db, &question, lookup, &provenance) {
            Ok(result) => result,
            Err(_) if lookup.is_some() => {
                coverage["unavailable"] = json!(true);
                coverage["partial"] = json!(true);
                return Ok((Vec::new(), coverage));
            }
            Err(error) => return Err(error),
        }
    } else {
        (None, 0)
    };
    coverage["catalog_lookups"] = json!(lookups);
    let mut additions = Vec::new();
    let mut checks = 0;
    let (mut frontier, seed_match, first_hop) = if let Some((id, edge)) = catalog {
        let score = seeds
            .first()
            .and_then(|seed| seed["score"].as_f64())
            .unwrap_or(0.0);
        let seed = edge.chunk.clone();
        let entity = if id == edge.subject_key {
            edge.object_key.clone()
        } else {
            edge.subject_key.clone()
        };
        checks = 1;
        traversed.insert(edge.id.clone());
        let node = Frontier {
            entity,
            chain: vec![seed.clone()],
            seed,
            score,
            contains_unverified: edge.assertion_status == EXTRACTED_UNVERIFIED,
        };
        if visited.insert(node.seed.clone()) {
            additions.push(graph_hit(edge, &node, 1, &node.chain, "catalog_id"));
        }
        (vec![node], "catalog_id", 2)
    } else if exact_frontier.is_empty() {
        (lexical_frontier, "lexical_coverage", 1)
    } else {
        (exact_frontier, "exact_entity", 1)
    };
    if !frontier.is_empty() {
        coverage["seed_status"] = json!("seeded");
        coverage["seed_match"] = json!(seed_match);
    }
    coverage["checks"] = json!(checks);
    if additions.len() == limit {
        coverage["partial"] = json!(true);
        coverage["result_limit_reached"] = json!(true);
        return Ok((additions, coverage));
    }
    for hop in first_hop..=MAX_HOPS {
        let mut next = Vec::new();
        for node in frontier {
            if !queried.insert(node.entity.clone()) {
                continue;
            }
            let remaining = MAX_CHECKS - checks;
            let rows = match neighbors(db, &node.entity, remaining + 1, lookup, &provenance) {
                Ok(rows) => rows,
                Err(_) if lookup.is_some() => {
                    coverage["unavailable"] = json!(true);
                    coverage["partial"] = json!(true);
                    coverage["checks"] = json!(0);
                    return Ok((Vec::new(), coverage));
                }
                Err(error) => return Err(error),
            };
            let exhausted = rows.len() > remaining;
            checks += rows.len().min(remaining);
            coverage["checks"] = json!(checks);
            for edge in rows.into_iter().take(remaining) {
                if !traversed.insert(edge.id.clone()) {
                    continue;
                }
                let mut chain = node.chain.clone();
                if !chain.contains(&edge.chunk) {
                    chain.push(edge.chunk.clone());
                }
                if hop < MAX_HOPS {
                    let entity = if node.entity == edge.subject_key {
                        edge.object_key.clone()
                    } else {
                        edge.subject_key.clone()
                    };
                    next.push(Frontier {
                        entity,
                        seed: node.seed.clone(),
                        chain: chain.clone(),
                        score: node.score,
                        contains_unverified: node.contains_unverified
                            || edge.assertion_status == EXTRACTED_UNVERIFIED,
                    });
                }
                if !visited.insert(edge.chunk.clone()) {
                    continue;
                }
                additions.push(graph_hit(edge, &node, hop, &chain, seed_match));
                if additions.len() == limit {
                    coverage["partial"] = json!(true);
                    coverage["result_limit_reached"] = json!(true);
                    coverage["traversal_budget_exhausted"] = json!(exhausted);
                    return Ok((additions, coverage));
                }
            }
            if exhausted {
                coverage["partial"] = json!(true);
                coverage["traversal_budget_exhausted"] = json!(true);
                return Ok((additions, coverage));
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    Ok((additions, coverage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::TransactionBehavior;

    #[cfg(feature = "model")]
    #[test]
    fn local_clause_context_requires_the_exact_indexed_revision_and_span() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.md");
        let prefix = "# Réseau\r\n\r\n";
        let text = "Gate-7 uses Relay-9.";
        let raw = format!("\u{feff}{prefix}{text}").into_bytes();
        std::fs::write(&path, &raw).unwrap();
        let mut passage = SourcePassage {
            path,
            source_hash: digest(&raw),
            start: prefix.chars().count() as i64,
            end: (prefix.chars().count() + text.chars().count()) as i64,
            text: text.into(),
        };
        let mut budget = 8192;
        assert_eq!(
            passage.read_current(directory.path(), &mut budget).unwrap(),
            format!("{prefix}{text}")
        );
        passage.start -= 1;
        assert!(passage.read_current(directory.path(), &mut budget).is_err());
        passage.start += 1;
        std::fs::write(&passage.path, b"Gate-7 uses Relay-8.").unwrap();
        assert!(passage.read_current(directory.path(), &mut budget).is_err());
    }

    fn index() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE sources(source_id TEXT PRIMARY KEY,path TEXT,source_hash TEXT);
             CREATE TABLE chunks(
                row_id INTEGER PRIMARY KEY,chunk_id TEXT UNIQUE,
                source_id TEXT REFERENCES sources(source_id) ON DELETE CASCADE,
                start INTEGER,end INTEGER,page INTEGER
             );
             CREATE TABLE passages(text TEXT);",
        )
        .unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }

    fn add_chunk(db: &Connection, id: &str, text: &str, offset: i64, page: Option<i64>) {
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,?3)",
            params![id, format!("{id}.md"), id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO chunks(chunk_id,source_id,start,end,page) VALUES(?1,?1,?2,?3,?4)",
            params![id, offset, offset + text.chars().count() as i64, page],
        )
        .unwrap();
        db.execute(
            "INSERT INTO passages(rowid,text) VALUES(?1,?2)",
            params![db.last_insert_rowid(), text],
        )
        .unwrap();
    }

    fn edge(subject: &str, object: &str, quote: &str) -> Value {
        json!({"subject": subject, "predicate": "uses", "object": object, "quote": quote})
    }

    fn vocab() -> [String; 1] {
        ["uses".into()]
    }

    #[cfg(feature = "shared")]
    fn shared_fixture(
        name: &str,
        raw: &[u8],
        text: &str,
        items: &[SharedItem],
    ) -> (Connection, tempfile::TempDir, String) {
        let mut db = index();
        let directory = tempfile::tempdir().unwrap();
        let path = crate::files::directory(directory.path())
            .unwrap()
            .join(name);
        std::fs::write(&path, raw).unwrap();
        let revision = digest(raw);
        add_chunk(&db, "shared", text, 0, None);
        db.execute(
            "UPDATE sources SET path=?1,source_hash=?2 WHERE source_id='shared'",
            params![crate::files::display_path(&path).unwrap(), revision],
        )
        .unwrap();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        configure(&tx, "shared-review-v1").unwrap();
        install_shared_review(&tx).unwrap();
        assert!(publish_shared(&tx, "shared", "shared-review-v1", &vocab(), items, 7).unwrap());
        tx.commit().unwrap();
        (db, directory, revision)
    }

    #[cfg(feature = "shared")]
    #[test]
    fn shared_review_pages_and_only_publishes_explicit_decisions() {
        let text =
            "Cedar Pump uses Relay Kit. Cedar Pump uses Test Relay. Cedar Pump uses Trial Switch.";
        let items = [
            SharedItem {
                value: edge("Cedar Pump", "Relay Kit", "Cedar Pump uses Relay Kit."),
                disposition: SharedDisposition::Automatic {
                    rule: "equipment-map".into(),
                },
            },
            SharedItem {
                value: edge("Cedar Pump", "Test Relay", "Cedar Pump uses Test Relay."),
                disposition: SharedDisposition::Review {
                    reason: "model_proposal",
                },
            },
            SharedItem {
                value: edge(
                    "Cedar Pump",
                    "Trial Switch",
                    "Cedar Pump uses Trial Switch.",
                ),
                disposition: SharedDisposition::Review {
                    reason: "model_proposal",
                },
            },
        ];
        let (mut db, folder, revision) = shared_fixture("shared.md", text.as_bytes(), text, &items);
        let published = |db: &Connection| {
            db.query_row("SELECT count(*) FROM graph_assertions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(published(&db), 1);
        let first = shared_review_queue(&db, "shared-review-v1", None, 1).unwrap();
        assert_eq!(first["has_more"], true);
        let after = first["next_cursor"].as_str().unwrap();
        assert_eq!(first["proposals"][0]["proposal_id"], after);
        let second = shared_review_queue(&db, "shared-review-v1", Some(after), 1).unwrap();
        assert_eq!(second["has_more"], false);
        assert!(second["next_cursor"].is_null());
        let rows = [&first["proposals"][0], &second["proposals"][0]];
        let by_object = |object| rows.iter().find(|row| row["object"] == object).unwrap();
        let approve = by_object("Test Relay");
        assert_eq!(approve["reason"], "model_proposal");
        assert_eq!(approve["policy_version"], 7);
        assert_eq!(approve["fingerprint"], "shared-review-v1");
        assert_eq!(approve["revision"], revision);
        let root = crate::files::directory(folder.path()).unwrap();
        let mut decision = SharedDecision {
            id: approve["proposal_id"].as_str().unwrap(),
            approve: true,
            revision: &revision,
            fingerprint: "shared-review-v1",
            policy_version: 8,
            actor: "reviewer-1",
            idempotency_key: "request-1",
        };
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let receipt = decide_shared(&tx, &root, &decision).unwrap();
        tx.commit().unwrap();
        assert_eq!(receipt["status"], "approved");
        assert_eq!(receipt["policy_version"], 8);
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(decide_shared(&tx, &root, &decision).unwrap(), receipt);
        decision.approve = false;
        assert_eq!(
            decide_shared(&tx, &root, &decision)
                .unwrap_err()
                .downcast_ref::<SharedReviewError>(),
            Some(&SharedReviewError::Conflict)
        );
        decision.approve = true;
        decision.actor = "reviewer-2";
        assert_eq!(
            decide_shared(&tx, &root, &decision)
                .unwrap_err()
                .downcast_ref::<SharedReviewError>(),
            Some(&SharedReviewError::Conflict)
        );
        decision.id = by_object("Trial Switch")["proposal_id"].as_str().unwrap();
        decision.approve = false;
        decision.idempotency_key = "request-2";
        assert_eq!(
            decide_shared(&tx, &root, &decision).unwrap()["status"],
            "dismissed"
        );
        assert_eq!(published(&tx), 2);
        assert_eq!(
            shared_review_queue(&tx, "shared-review-v1", None, 2).unwrap()["proposals"],
            json!([])
        );
        let provenance: Vec<_> = tx.prepare(
            "SELECT policy_version,rule,reviewer FROM graph_edge_provenance ORDER BY policy_version",
        ).unwrap().query_map([], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?,
        ))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(
            provenance,
            vec![
                (7, Some("equipment-map".into()), None),
                (8, None, Some("reviewer-1".into())),
            ]
        );
        configure(&tx, "legacy").unwrap();
        assert_eq!(published(&tx), 0);
        assert_eq!(
            decide_shared(&tx, &root, &decision)
                .unwrap_err()
                .downcast_ref::<SharedReviewError>(),
            Some(&SharedReviewError::Stale)
        );
        tx.commit().unwrap();
    }

    #[cfg(feature = "shared")]
    #[test]
    fn shared_approval_rechecks_raw_binary_revision_before_publishing() {
        let text = "Cedar Pump uses Relay Kit.";
        let (mut db, folder, revision) = shared_fixture(
            "shared.pdf",
            b"%PDF-1.7\n%\xFF\xFE\n",
            text,
            &[SharedItem {
                value: edge("Cedar Pump", "Relay Kit", text),
                disposition: SharedDisposition::Review {
                    reason: "model_proposal",
                },
            }],
        );
        let root = crate::files::directory(folder.path()).unwrap();
        let queue = shared_review_queue(&db, "shared-review-v1", None, 1).unwrap();
        let mut decision = SharedDecision {
            id: queue["proposals"][0]["proposal_id"].as_str().unwrap(),
            approve: true,
            revision: &revision,
            fingerprint: "shared-review-v1",
            policy_version: 7,
            actor: "reviewer",
            idempotency_key: "request",
        };
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            decide_shared(&tx, &root, &decision).unwrap()["status"],
            "approved"
        );
        tx.rollback().unwrap();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let stale = |decision: &SharedDecision<'_>| {
            assert_eq!(
                decide_shared(&tx, &root, decision)
                    .unwrap_err()
                    .downcast_ref::<SharedReviewError>(),
                Some(&SharedReviewError::Stale)
            )
        };
        decision.revision = "wrong-revision";
        stale(&decision);
        decision.revision = &revision;
        decision.fingerprint = "retired-extractor";
        stale(&decision);
        decision.fingerprint = "shared-review-v1";
        std::fs::write(root.join("shared.pdf"), b"%PDF-1.7\n%\xFF\xFA\n").unwrap();
        stale(&decision);
        assert_eq!(
            shared_review_queue(&tx, "shared-review-v1", None, 1).unwrap(),
            queue
        );
        let count: i64 = tx
            .query_row("SELECT count(*) FROM graph_assertions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[cfg(feature = "model")]
    #[test]
    fn local_review_binds_confirmation_to_the_pending_source_revision() {
        let mut db = index();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.md");
        let text = "Cedar Pump uses Relay Kit. Cedar Pump uses Test Relay.";
        std::fs::write(&path, text).unwrap();
        add_chunk(&db, "a", text, 0, None);
        db.execute_batch("CREATE TABLE settings(id INTEGER PRIMARY KEY,root TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO settings VALUES(1,?1)",
            [directory.path().to_str().unwrap()],
        )
        .unwrap();
        db.execute(
            "UPDATE sources SET path=?1,source_hash=?2 WHERE source_id='a'",
            params![path.to_str().unwrap(), digest(text.as_bytes())],
        )
        .unwrap();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let fingerprint = format!("{LOCAL_REVIEW_FINGERPRINT_PREFIX}fixture");
        configure(&tx, &fingerprint).unwrap();
        install_review_queue(&tx).unwrap();
        assert!(
            stage(
                &tx,
                "a",
                &fingerprint,
                &vocab(),
                &[
                    edge("Cedar Pump", "Relay Kit", "Cedar Pump uses Relay Kit."),
                    edge("Cedar Pump", "Test Relay", "Cedar Pump uses Test Relay."),
                ]
            )
            .unwrap()
        );
        tx.commit().unwrap();

        let provenance = ProvenanceSql::read(&db).unwrap();
        assert!(
            seed_entities(&db, "a", "", &provenance)
                .unwrap()
                .0
                .is_empty()
        );
        let limited = review_queue(&db, 1).unwrap();
        assert_eq!(limited["has_more"], true);
        assert_eq!(
            limited["proposals"][0]["quote"],
            "Cedar Pump uses Relay Kit."
        );
        let approved = limited["proposals"][0]["proposal_id"].as_str().unwrap();
        let remaining = review_queue(&db, 2).unwrap();
        let dismissed = remaining["proposals"][1]["proposal_id"].as_str().unwrap();

        std::fs::write(&path, "Cedar Pump uses Other Relay.").unwrap();
        assert!(decide_proposal(&mut db, approved, true).is_err());
        assert!(
            seed_entities(&db, "a", "", &provenance)
                .unwrap()
                .0
                .is_empty()
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            decide_proposal(&mut db, approved, true).unwrap()["status"],
            "approved"
        );
        assert!(decide_proposal(&mut db, approved, true).is_err());
        assert_eq!(
            decide_proposal(&mut db, dismissed, false).unwrap()["status"],
            "dismissed"
        );
        assert!(decide_proposal(&mut db, dismissed, true).is_err());
        assert_eq!(review_queue(&db, 2).unwrap()["proposals"], json!([]));
        let quote: String = db
            .query_row("SELECT quote FROM graph_assertions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(quote, "Cedar Pump uses Relay Kit.");
        let reviewed = neighbors(&db, "cedar pump", 1, None, &provenance).unwrap();
        assert_eq!(reviewed[0].assertion_status, "reviewed");
        db.execute("DELETE FROM sources WHERE source_id='a'", [])
            .unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM graph_assertions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    fn seed(strong: bool) -> Value {
        json!({"chunk_id": "a", "score": 1.0, "content_coverage": {"passes": strong}})
    }

    #[test]
    fn graph_direction_follows_the_source_clause_and_preserves_quote_offsets() {
        for (predicate, subject, object, quote) in [
            (
                "uses",
                "North Bay",
                "Ice Sensor",
                "The Ice Sensor is used by North Bay.",
            ),
            (
                "requires",
                "Line 9",
                "Lock Kit",
                "The Lock Kit is required by Line 9.",
            ),
            (
                "depends_on",
                "Ember Board",
                "Orchid Relay",
                "Ember Board depends on the Orchid Relay.",
            ),
            (
                "owned_by",
                "Ivory Crane",
                "Harbor Works",
                "Harbor Works owns the Ivory Crane.",
            ),
            (
                "owned_by",
                "Copper Rig",
                "Quartz Guild",
                "Copper Rig is owned by Quartz Guild.",
            ),
            (
                "part_of",
                "Linden Latch",
                "Kestrel Assembly",
                "The Linden Latch is a component of the Kestrel Assembly.",
            ),
        ] {
            let passage = format!("🦉\n{quote}\nFiled.");
            let predicates = [predicate.into()];
            let mut db = index();
            add_chunk(&db, "direction", &passage, 12, None);
            let tx = db.transaction().unwrap();
            configure(&tx, "direction").unwrap();
            let correct = json!({"subject": subject, "predicate": predicate, "object": object, "quote": quote});
            let reversed = json!({"subject": object, "predicate": predicate, "object": subject, "quote": quote});
            assert!(
                publish(
                    &tx,
                    "direction",
                    "direction",
                    &predicates,
                    &[reversed, correct]
                )
                .unwrap()
            );
            let row: (String, String, String, i64) = tx
                .query_row(
                    "SELECT subject,object,quote,quote_start FROM graph_assertions",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(row, (subject.into(), object.into(), quote.into(), 14));
            assert_eq!(
                tx.query_row("SELECT count(*) FROM graph_assertions", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn graph_direction_keeps_ambiguous_or_custom_prose_unverified() {
        for (predicate, quote, before, after) in [
            ("uses", "Beta does not use Alpha.", "", ""),
            ("uses", "Beta uses Alpha.", "If ", " Check the log."),
            ("uses", "Beta uses Alpha", "", ", according to a draft."),
            ("uses", "Beta uses Alpha.", "", "example"),
            ("uses", "Beta uses Alpha", "", ".example"),
            ("uses", "Beta uses Alpha.", "PREFIX.", ""),
            ("uses", "Beta uses Alpha and Alpha uses Beta.", "", ""),
            ("uses", "Beta uses Alpha. Alpha uses Beta.", "", ""),
            ("uses", "Beta uses Alpha annually.", "", ""),
            ("part_of", "Beta contains Alpha.", "", ""),
            ("custom", "Beta uses Alpha.", "", ""),
        ] {
            assert_eq!(
                direction_support(predicate, "alpha", "beta", quote, before, after),
                DirectionSupport::Unknown
            );
        }
        for (quote, before, after) in [
            ("Beta uses Alpha.", "", " But the log is offline."),
            ("Beta uses Alpha", "A prior note. ", ". A later note."),
        ] {
            assert_eq!(
                direction_support("uses", "alpha", "beta", quote, before, after),
                DirectionSupport::Reversed
            );
        }
    }

    #[test]
    fn graph_direction_flags_legacy_assertions_without_changing_the_stored_tuple() {
        let quote = "The Honey Hoist is owned by Alder Works.";
        let text = format!("É\n{quote}");
        let evidence = json!({"start": 19, "text": text});
        let assertion = json!({"edge_id": "legacy-id", "subject": "Alder Works", "predicate": "owned_by",
            "object": "Honey Hoist", "quote": quote, "start": 21, "end": 21 + quote.chars().count()});
        let edge = Edge {
            id: "legacy-id".into(),
            subject_key: "alder works".into(),
            object_key: "honey hoist".into(),
            chunk: "legacy-chunk".into(),
            assertion_status: EXTRACTED_UNVERIFIED.into(),
            assertion,
            evidence,
        };
        let node = Frontier {
            entity: "alder works".into(),
            seed: "seed".into(),
            score: 1.0,
            chain: Vec::new(),
            contains_unverified: false,
        };
        let hit = graph_hit(edge, &node, 1, &[], "lexical_coverage");
        let graph = &hit["graph_support"];
        assert_eq!(graph["direction_support"], "contradicted_by_source_clause");
        assert_eq!(graph["assertion_status"], "extracted_unverified");
        assert_eq!(graph["assertion"]["edge_id"], "legacy-id");
        assert_eq!(graph["assertion"]["subject"], "Alder Works");
        assert_eq!(graph["assertion"]["object"], "Honey Hoist");
        assert_eq!(graph["source_direction"]["subject"], "Honey Hoist");
        assert_eq!(graph["source_direction"]["object"], "Alder Works");
        assert_eq!(graph["assertion"]["quote"], quote);
        assert_eq!(graph["assertion"]["start"], 21);
    }

    #[test]
    fn two_hops_are_anchored_and_stale_chunks_cannot_be_published() {
        let mut db = index();
        let first = "CedarPump uses RelayKit.";
        let second = "RelayKit uses SpareBracket.";
        let third = "SpareBracket uses WirePin.";
        add_chunk(&db, "a", first, 0, None);
        add_chunk(&db, "b", &format!("🦉 {second}"), 12, Some(2));
        add_chunk(&db, "c", third, 0, None);
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        configure(&tx, "fixture").unwrap();
        assert_eq!(pending(&tx, "fixture", 3).unwrap(), ["a", "b", "c"]);
        assert_eq!(
            unfinished_text(&tx, "a", "fixture").unwrap().as_deref(),
            Some(first)
        );
        for (id, assertion) in [
            ("a", edge("CedarPump", "RelayKit", first)),
            ("b", edge("RelayKit", "SpareBracket", second)),
            ("c", edge("SpareBracket", "WirePin", third)),
        ] {
            assert!(publish(&tx, id, "fixture", &vocab(), &[assertion]).unwrap());
        }
        assert!(pending(&tx, "fixture", 3).unwrap().is_empty());
        assert!(unfinished_text(&tx, "a", "fixture").unwrap().is_none());
        tx.commit().unwrap();

        let tx = db.transaction().unwrap();
        let (weak, _) = expand(&tx, &[seed(false)], "", 5).unwrap();
        assert!(weak.is_empty());
        let (filled, coverage) = expand(&tx, &[seed(true)], "", 0).unwrap();
        assert!(filled.is_empty());
        assert_eq!(coverage["result_limit_reached"], true);
        let (hits, coverage) = expand(&tx, &[seed(true)], "", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["page"], 2);
        assert_eq!(hits[0]["graph_support"]["assertion"]["quote"], second);
        assert_eq!(hits[0]["graph_support"]["assertion"]["start"], 14);
        assert!(hits.iter().all(|hit| {
            hit["graph_support"]["assertion_status"] == EXTRACTED_UNVERIFIED
                && hit["graph_support"]["path_contains_unverified_assertion"] == true
        }));
        assert_eq!(hits[1]["graph_support"]["hop"], 2);
        assert_eq!(
            hits[1]["graph_support"]["supporting_chunk_ids"],
            json!(["a", "b", "c"])
        );
        assert_eq!(coverage["partial"], false);
        let provenance = ProvenanceSql::read(&tx).unwrap();
        let lookup = |entity: &str, limit: usize| {
            Ok(neighbors(&tx, entity, limit, None, &provenance)?
                .into_iter()
                .map(|edge| edge.id)
                .collect())
        };
        let (projected, coverage) = expand_external(&tx, &[seed(true)], "", 5, &lookup).unwrap();
        assert_eq!(projected, hits);
        assert_eq!(coverage["backend"], "neptune");

        // A remote edge from the wrong entity in a later hop invalidates the
        // entire expansion. No candidates or check counts from earlier hops
        // escape when the scoped projection cannot be trusted.
        let wrong = neighbors(&tx, "cedarpump", 1, None, &provenance).unwrap()[0]
            .id
            .clone();
        let stale = |entity: &str, limit: usize| {
            if entity == "sparebracket" {
                return Ok(vec![wrong.clone()]);
            }
            lookup(entity, limit)
        };
        let (discarded, coverage) = expand_external(&tx, &[seed(true)], "", 5, &stale).unwrap();
        assert!(discarded.is_empty());
        assert_eq!(coverage["unavailable"], true);
        assert_eq!(coverage["checks"], 0);
        tx.commit().unwrap();

        #[cfg(feature = "shared")]
        {
            let tx = db.transaction().unwrap();
            install_shared_review(&tx).unwrap();
            tx.execute(
                "INSERT INTO graph_edge_provenance(edge_id,policy_version,reviewer)
                 SELECT edge_id,1,'reviewer' FROM graph_assertions WHERE subject_key='sparebracket'",
                [],
            )
            .unwrap();
            let (hits, _) = expand(&tx, &[seed(true)], "", 5).unwrap();
            assert_eq!(hits[1]["graph_support"]["assertion_status"], "reviewed");
            assert_eq!(
                hits[1]["graph_support"]["path_contains_unverified_assertion"],
                true
            );
            tx.commit().unwrap();
        }

        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "DELETE FROM passages WHERE rowid=(SELECT row_id FROM chunks WHERE chunk_id='b')",
            [],
        )
        .unwrap();
        tx.execute("DELETE FROM sources WHERE source_id='b'", [])
            .unwrap();
        assert!(unfinished_text(&tx, "b", "fixture").unwrap().is_none());
        assert!(
            !publish(
                &tx,
                "b",
                "fixture",
                &vocab(),
                &[edge("RelayKit", "SpareBracket", second)]
            )
            .unwrap()
        );
        assert!(expand(&tx, &[seed(true)], "", 5).unwrap().0.is_empty());
        tx.commit().unwrap();
    }

    #[test]
    fn weak_seed_requires_a_precise_entity_and_expands_its_assertion_only() {
        let mut db = index();
        let first = "Cedar Pump requires Relay Pack. Guard Rail uses Survey Tool.";
        let second = "Relay Pack requires Fuse XR-204.";
        let unrelated = "Survey Tool uses Tripod Arm.";
        add_chunk(&db, "a", first, 0, None);
        add_chunk(&db, "b", second, 0, None);
        add_chunk(&db, "c", unrelated, 0, None);
        let tx = db.transaction().unwrap();
        configure(&tx, "fixture").unwrap();
        assert!(
            publish(
                &tx,
                "a",
                "fixture",
                &vocab(),
                &[
                    edge(
                        "Cedar Pump",
                        "Relay Pack",
                        "Cedar Pump requires Relay Pack."
                    ),
                    edge("Guard Rail", "Survey Tool", "Guard Rail uses Survey Tool."),
                ]
            )
            .unwrap()
        );
        assert!(
            publish(
                &tx,
                "b",
                "fixture",
                &vocab(),
                &[edge("Relay Pack", "Fuse XR-204", second)]
            )
            .unwrap()
        );
        assert!(
            publish(
                &tx,
                "c",
                "fixture",
                &vocab(),
                &[edge("Survey Tool", "Tripod Arm", unrelated)]
            )
            .unwrap()
        );

        let query = "What can keep the cedar pump operating reliably during restart?";
        let (hits, coverage) = expand(&tx, &[seed(false)], query, 5).unwrap();
        assert_eq!(coverage["seed_match"], "exact_entity");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["chunk_id"], "b");
        assert_eq!(hits[0]["graph_support"]["assertion"]["quote"], second);
        assert_eq!(
            hits[0]["graph_support"]["supporting_chunk_ids"],
            json!(["a", "b"])
        );
        for weak in [
            "What keeps the pump running?",
            "Does Cedar Pumps Plus work?",
        ] {
            assert!(expand(&tx, &[seed(false)], weak, 5).unwrap().0.is_empty());
        }
        assert!(precise_mention("replace xr-204 now", "xr-204"));
        assert!(!precise_mention("replace xr-204-b now", "xr-204"));

        let mut later = vec![seed(false); MAX_SEEDS];
        later.push(seed(true));
        let (hits, coverage) = expand(&tx, &later, "Unrelated wording", 5).unwrap();
        assert_eq!(coverage["seed_match"], "lexical_coverage");
        assert!(hits.iter().any(|hit| hit["chunk_id"] == "b"));
    }

    #[test]
    fn catalog_lookup_recovers_a_ranked_out_path_with_active_anchors() {
        let mut db = index();
        let first = "XN-716 requires Blue Pin manifold.";
        let second = "Blue Pin manifold requires Wattle regulator.";
        let contextual = "YY-033 requires Cabinet Key.";
        add_chunk(&db, "a", first, 0, None);
        add_chunk(&db, "b", second, 0, None);
        add_chunk(&db, "c", "Winter pressure and automatic purge.", 0, None);
        add_chunk(&db, "d", contextual, 0, None);
        let tx = db.transaction().unwrap();
        configure(&tx, "catalog").unwrap();
        assert!(
            publish(
                &tx,
                "a",
                "catalog",
                &vocab(),
                &[edge("XN-716", "Blue Pin manifold", first)]
            )
            .unwrap()
        );
        assert!(
            publish(
                &tx,
                "d",
                "catalog",
                &vocab(),
                &[edge("YY-033", "Cabinet Key", contextual)]
            )
            .unwrap()
        );
        assert!(
            publish(
                &tx,
                "b",
                "catalog",
                &vocab(),
                &[edge("Blue Pin manifold", "Wattle regulator", second)]
            )
            .unwrap()
        );
        let seed = json!({"chunk_id": "c", "score": 0.25, "content_coverage": {"passes": false}});
        let seeds = &[seed];
        let query = "Which backup protects XN-716 during the automatic winter purge?";
        let (hits, coverage) = expand(&tx, seeds, query, 2).unwrap();
        assert_eq!(coverage["seed_match"], "catalog_id");
        assert_eq!(coverage["catalog_lookups"], 1);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["graph_support"]["assertion"]["quote"], first);
        assert_eq!(hits[0]["graph_support"]["hop"], 1);
        assert_eq!(hits[1]["graph_support"]["assertion"]["quote"], second);
        assert_eq!(hits[1]["graph_support"]["hop"], 2);
        assert_eq!(
            hits[1]["graph_support"]["supporting_chunk_ids"],
            json!(["a", "b"])
        );

        let ranked =
            [json!({"chunk_id": "a", "score": 0.25, "content_coverage": {"passes": false}})];
        let (contextual_hits, coverage) = expand(
            &tx,
            &ranked,
            "Which regulator protects XN-716 during YY-033 maintenance?",
            2,
        )
        .unwrap();
        assert_eq!(coverage["seed_match"], "exact_entity");
        assert_eq!(coverage["catalog_lookups"], 0);
        assert_eq!(contextual_hits.len(), 1);
        assert_eq!(contextual_hits[0]["chunk_id"], "b");

        let provenance = ProvenanceSql::read(&tx).unwrap();
        let lookup = |entity: &str, limit: usize| {
            Ok(neighbors(&tx, entity, limit, None, &provenance)?
                .into_iter()
                .map(|edge| edge.id)
                .collect())
        };
        assert_eq!(
            expand_external(&tx, seeds, query, 2, &lookup).unwrap().0,
            hits
        );
        let outside = |_entity: &str, _limit: usize| lookup("wattle regulator", 1);
        let (discarded, coverage) = expand_external(&tx, seeds, query, 2, &outside).unwrap();
        assert!(discarded.is_empty());
        assert_eq!(coverage["unavailable"], true);

        let (missing, coverage) = expand(&tx, seeds, "AA-01 BB-02 CC-03 DD-04 XN-716", 2).unwrap();
        assert!(missing.is_empty());
        assert_eq!(coverage["catalog_lookups"], MAX_CATALOG_IDS);
        assert!(
            expand(&tx, seeds, "Replace XN-716-B", 2)
                .unwrap()
                .0
                .is_empty()
        );

        tx.execute(
            "DELETE FROM passages WHERE rowid=(SELECT row_id FROM chunks WHERE chunk_id='a')",
            [],
        )
        .unwrap();
        tx.execute("DELETE FROM sources WHERE source_id='a'", [])
            .unwrap();
        assert!(expand(&tx, seeds, query, 2).unwrap().0.is_empty());
    }

    #[test]
    fn malformed_ambiguous_and_unanchored_assertions_leave_the_previous_graph_intact() {
        let mut db = index();
        let quote = "Alpha uses Beta.";
        let full = format!("{quote} {quote}");
        add_chunk(&db, "a", &full, 0, None);
        add_chunk(&db, "b", &full, 0, None);
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        configure(&tx, "before").unwrap();
        let valid = edge("Alpha", "Beta", &full);
        assert!(
            publish(
                &tx,
                "a",
                "before",
                &vocab(),
                &[valid.clone(), valid.clone()]
            )
            .unwrap()
        );
        assert!(!publish(&tx, "a", "before", &vocab(), &[]).unwrap());
        for bad in [
            edge("Alpha", "Beta", quote),
            edge("Alph", "Beta", &full),
            edge("Alpha", "alpha", &full),
            edge("Alpha", "Beta", "Beta uses Alpha."),
            json!({"subject": "Alpha", "predicate": "depends_on", "object": "Beta", "quote": full}),
            json!({"subject": "Alpha", "predicate": "uses", "object": "Beta", "quote": full, "extra": 0}),
        ] {
            assert!(publish(&tx, "b", "before", &vocab(), &[valid.clone(), bad]).is_err());
        }
        let count: i64 = tx
            .query_row("SELECT count(*) FROM graph_assertions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(pending(&tx, "before", 1).unwrap(), ["b"]);

        configure(&tx, "after").unwrap();
        assert!(unfinished_text(&tx, "b", "before").unwrap().is_none());
        assert_eq!(
            unfinished_text(&tx, "b", "after").unwrap().as_deref(),
            Some(full.as_str())
        );
        assert!(!publish(&tx, "a", "before", &vocab(), &[valid]).unwrap());
        assert_eq!(pending(&tx, "after", 2).unwrap(), ["a", "b"]);
        assert!(publish(&tx, "a", "after", &vocab(), &[]).unwrap());
        assert_eq!(pending(&tx, "after", 2).unwrap(), ["b"]);
        let provenance = ProvenanceSql::read(&tx).unwrap();
        assert!(
            seed_entities(&tx, "a", "", &provenance)
                .unwrap()
                .0
                .is_empty()
        );
        tx.commit().unwrap();
    }

    #[test]
    fn traversal_stops_at_the_edge_check_budget_even_if_all_hits_are_duplicates() {
        let mut db = index();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        configure(&tx, "fixture").unwrap();
        let records: Vec<_> = (0..MAX_ASSERTIONS)
            .map(|index| {
                let target = format!("Terminal{index}");
                edge("Hub", &target, &format!("Hub uses {target}."))
            })
            .collect();
        let text = records
            .iter()
            .map(|record| record["quote"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        let mut seeds = Vec::new();
        for index in 0..=(MAX_CHECKS / MAX_ASSERTIONS) {
            let id = format!("source{index}");
            add_chunk(&tx, &id, &text, 0, None);
            assert!(publish(&tx, &id, "fixture", &vocab(), &records).unwrap());
            seeds.push(json!({
                "chunk_id": id, "score": 1.0, "content_coverage": {"passes": index == 0}
            }));
        }
        let (hits, coverage) = expand(&tx, &seeds, "", 2).unwrap();
        assert!(hits.is_empty());
        assert_eq!(coverage["checks"], MAX_CHECKS);
        assert_eq!(coverage["traversal_budget_exhausted"], true);
        assert_eq!(coverage["partial"], true);
    }
}
