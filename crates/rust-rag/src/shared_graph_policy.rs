//! Compile exact declarations from active local source revisions before graph publication.

use crate::{
    files,
    graph::{SharedDisposition, SharedItem, entity_key},
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

const MARKER: &str = "anyrag-relations";
const OPENER: &str = "```anyrag-relations";
const CLOSER: &str = "```";
const ROUTINE_RULE: &str = "declared-routine-v1";
const MAX_DOCUMENTS: usize = 1_024;
const MAX_DECLARATIONS: usize = 8_192;
const MAX_PER_CHUNK: usize = 16;
const MAX_QUOTE: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationPolicy {
    pub(crate) trusted_paths: Vec<String>,
    pub(crate) routine_predicates: Vec<String>,
    pub(crate) high_impact_predicates: Vec<String>,
    #[serde(default)]
    pub(crate) single_valued_predicates: Vec<String>,
}

fn text_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("txt"))
}

fn safe_relative(path: &str) -> bool {
    path.chars().count() <= 1_024
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part.trim() == part
                && !part.ends_with('.')
        })
        && text_path(Path::new(path))
}

impl PublicationPolicy {
    pub(crate) fn validate(&self, predicates: &[String]) -> Result<()> {
        ensure!(
            self.trusted_paths.len() <= MAX_DOCUMENTS
                && self.trusted_paths.iter().all(|path| safe_relative(path))
                && self.trusted_paths.iter().collect::<BTreeSet<_>>().len()
                    == self.trusted_paths.len(),
            "Trusted declaration paths must be distinct collection-relative Markdown or text paths."
        );
        for terms in [
            &self.routine_predicates,
            &self.high_impact_predicates,
            &self.single_valued_predicates,
        ] {
            ensure!(
                terms.iter().all(|term| predicates.contains(term))
                    && terms.iter().collect::<BTreeSet<_>>().len() == terms.len(),
                "Publication predicates must be distinct members of the configured graph vocabulary."
            );
        }
        ensure!(
            self.routine_predicates
                .iter()
                .all(|term| !self.high_impact_predicates.contains(term)),
            "A publication predicate cannot be both routine and high impact."
        );
        Ok(())
    }

    pub(crate) fn digest(&self) -> Result<String> {
        let ordered = |values: &[String]| values.iter().cloned().collect::<BTreeSet<_>>();
        Ok(files::digest(&serde_json::to_vec(&json!([
            ROUTINE_RULE,
            ordered(&self.trusted_paths),
            ordered(&self.routine_predicates),
            ordered(&self.high_impact_predicates),
            ordered(&self.single_valued_predicates),
        ]))?))
    }

    pub(crate) fn stage_proposals(&self, proposals: Vec<Value>) -> Vec<SharedItem> {
        proposals
            .into_iter()
            .map(|value| {
                let reason = if value["predicate"].as_str().is_some_and(|predicate| {
                    self.high_impact_predicates
                        .iter()
                        .any(|term| term == predicate)
                }) {
                    "high_impact_predicate"
                } else {
                    "model_only"
                };
                SharedItem {
                    value,
                    disposition: SharedDisposition::Review { reason },
                }
            })
            .collect()
    }
}

#[derive(Debug)]
struct Declaration {
    value: Value,
    start: usize,
    end: usize,
    valid_block: bool,
}

fn declaration(line: &str, start: usize, predicates: &[String]) -> Option<Declaration> {
    let mut fields = line.split('|').map(str::trim);
    let (subject, predicate, object) = (fields.next()?, fields.next()?, fields.next()?);
    let length = line.chars().count();
    if fields.next().is_some()
        || length > MAX_QUOTE
        || !predicates.iter().any(|allowed| allowed == predicate)
        || [subject, object].iter().any(|name| {
            name.is_empty() || name.chars().count() > 160 || name.chars().any(char::is_control)
        })
        || entity_key(subject) == entity_key(object)
    {
        return None;
    }
    Some(Declaration {
        value: json!({"subject":subject, "predicate":predicate, "object":object, "quote":line}),
        start,
        end: start + length,
        valid_block: false,
    })
}

fn fence(line: &str) -> Option<(u8, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = *trimmed.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = trimmed.bytes().take_while(|&byte| byte == marker).count();
    (length >= 3).then(|| (marker, length, &trimmed[length..]))
}

fn comment_state(mut line: &str, mut open: bool) -> bool {
    loop {
        let delimiter = if open { "-->" } else { "<!--" };
        let Some((_, rest)) = line.split_once(delimiter) else {
            return open;
        };
        open = !open;
        line = rest;
    }
}

fn parse(document: &str, predicates: &[String], limit: usize) -> Result<Vec<Declaration>> {
    let mut rows = Vec::<Declaration>::new();
    let mut block = None;
    let mut other_fence = None;
    let mut comment = false;
    let mut offset = 0;
    for raw in document.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let start = offset;
        offset += raw.chars().count();
        if let Some((begin, valid)) = block.as_mut() {
            if line == CLOSER {
                if *valid {
                    for row in &mut rows[*begin..] {
                        row.valid_block = true;
                    }
                }
                block = None;
            } else if let Some(row) = declaration(line, start, predicates) {
                ensure!(
                    rows.len() < limit,
                    "The declaration catalog exceeds its row limit."
                );
                rows.push(row);
            } else {
                *valid = false;
            }
            continue;
        }
        if let Some((marker, length)) = other_fence {
            if fence(line).is_some_and(|(end, count, tail)| {
                end == marker && count >= length && tail.trim().is_empty()
            }) {
                other_fence = None;
            }
            continue;
        }
        if comment {
            comment = comment_state(line, true);
        } else if line == OPENER {
            block = Some((rows.len(), true));
        } else if let Some((marker, length, _)) = fence(line) {
            other_fence = Some((marker, length));
        } else {
            comment = comment_state(line, false);
        }
    }
    Ok(rows)
}

struct Chunk {
    id: String,
    start: usize,
    end: usize,
    text: String,
}

fn chunks(db: &Connection, source_id: &str, document: &str) -> Result<Vec<Chunk>> {
    let positions: Vec<_> = document
        .char_indices()
        .map(|(index, _)| index)
        .chain([document.len()])
        .collect();
    let mut statement = db.prepare(
        "SELECT c.chunk_id,c.start,c.end,p.text
         FROM chunks c JOIN passages p ON p.rowid=c.row_id
         WHERE c.source_id=?1 ORDER BY c.start,c.end,c.chunk_id",
    )?;
    let records = statement.query_map([source_id], |row| {
        Ok((
            row.get(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get(3)?,
        ))
    })?;
    let mut chunks = Vec::new();
    for row in records {
        let (id, start, end, text) = row?;
        let row = Chunk {
            id,
            start: usize::try_from(start)?,
            end: usize::try_from(end)?,
            text,
        };
        ensure!(
            row.start < row.end
                && row.end < positions.len()
                && document[positions[row.start]..positions[row.end]] == row.text,
            "An indexed declaration passage differs from its active source."
        );
        chunks.push(row);
    }
    Ok(chunks)
}

fn owner<'a>(row: &Declaration, chunks: &'a [Chunk]) -> Option<&'a str> {
    let quote = row.value["quote"].as_str().expect("declaration quote");
    // Chunks arrive in offset order. Choose the first one that fully and uniquely owns the line.
    chunks
        .iter()
        .take_while(|chunk| chunk.start <= row.start)
        .find_map(|chunk| {
            if row.end > chunk.end {
                return None;
            }
            let position = chunk.text.find(quote)?;
            let after_first = position + quote.chars().next()?.len_utf8();
            (chunk.start + chunk.text[..position].chars().count() == row.start
                && !chunk.text[after_first..].contains(quote))
            .then_some(chunk.id.as_str())
        })
}

fn indexed_path<'a>(root: &'a Path, indexed: &'a Path) -> Result<&'a Path> {
    let relative = indexed
        .strip_prefix(root)
        .context("A declaration source is outside its collection root.")?;
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "A declaration source does not have a safe collection-relative path."
    );
    Ok(relative)
}

struct Edge {
    value: Value,
    chunk: String,
    path: String,
    start: usize,
    end: usize,
    valid_block: bool,
    subject_key: String,
    object_key: String,
}

impl Edge {
    fn predicate(&self) -> &str {
        self.value["predicate"]
            .as_str()
            .expect("declaration predicate")
    }
}

fn load_edges(db: &Connection, root: &Path, predicates: &[String]) -> Result<Vec<Edge>> {
    ensure!(
        root.is_absolute(),
        "Declaration sources require a local absolute collection root."
    );
    let stamp = files::folder_stamp(root)?;
    let root_display = Path::new(files::display_path(root)?);
    let mut statement = db.prepare(
        r#"SELECT DISTINCT s.source_id,s.path,s.source_hash
         FROM passages p JOIN chunks c ON c.row_id=p.rowid JOIN sources s USING(source_id)
         WHERE passages MATCH '"anyrag-relations"' AND instr(p.text,?1)>0
         AND (lower(substr(s.path,-3))='.md' OR lower(substr(s.path,-4))='.txt')
         ORDER BY s.path LIMIT ?2"#,
    )?;
    // The 150-character text chunk overlap exceeds the complete marker length.
    let sources = statement.query_map((MARKER, (MAX_DOCUMENTS + 1) as i64), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut edges = Vec::new();
    let mut remaining = files::MAX_BATCH_BYTES;
    for (index, source) in sources.enumerate() {
        ensure!(
            index < MAX_DOCUMENTS,
            "The declaration catalog exceeds its source limit."
        );
        let (source_id, path, source_hash) = source?;
        let relative = indexed_path(root_display, Path::new(&path))?;
        let relative_name = files::display_path(relative)?.replace(std::path::MAIN_SEPARATOR, "/");
        let local = root.join(relative);
        let file = files::file_stamp(&local)?;
        let raw = files::read_revision(&local, &mut remaining, &file)?;
        ensure!(
            files::digest(&raw) == source_hash,
            "A declaration source changed. Sync its collection first."
        );
        let document = files::decode(&raw)?;
        let parsed = parse(&document, predicates, MAX_DECLARATIONS - edges.len())?;
        if parsed.is_empty() {
            continue;
        }
        let active = chunks(db, &source_id, &document)?;
        for row in parsed {
            let chunk = owner(&row, &active).context(
                "A declaration needs a unique exact quote wholly in one active passage.",
            )?;
            edges.push(Edge {
                chunk: chunk.to_owned(),
                path: relative_name.clone(),
                start: row.start,
                end: row.end,
                valid_block: row.valid_block,
                subject_key: entity_key(
                    row.value["subject"].as_str().expect("declaration subject"),
                ),
                object_key: entity_key(row.value["object"].as_str().expect("declaration object")),
                value: row.value,
            });
        }
    }
    ensure!(
        files::folder_stamp(root)? == stamp,
        "The declaration source root changed during scanning."
    );
    Ok(edges)
}

struct Routed {
    value: Value,
    reason: Option<&'static str>,
    path: String,
    start: usize,
    end: usize,
}

struct DeclarationChunk {
    rows: Vec<Routed>,
    digest: String,
}

pub(crate) struct Catalog {
    chunks: BTreeMap<String, DeclarationChunk>,
    digest: String,
}

fn route(edges: Vec<Edge>, policy: &PublicationPolicy) -> Result<Catalog> {
    let pairs: BTreeSet<_> = edges
        .iter()
        .map(|edge| (edge.subject_key.as_str(), edge.object_key.as_str()))
        .collect();
    let mut targets = BTreeMap::<_, BTreeSet<_>>::new();
    for edge in &edges {
        if policy
            .single_valued_predicates
            .iter()
            .any(|term| term == edge.predicate())
        {
            targets
                .entry((edge.subject_key.as_str(), edge.predicate()))
                .or_default()
                .insert(edge.object_key.as_str());
        }
    }
    let reasons: Vec<_> = edges
        .iter()
        .map(|edge| {
            if !edge.valid_block {
                Some("invalid_declaration_block")
            } else if !policy.trusted_paths.contains(&edge.path) {
                Some("untrusted_declaration_path")
            } else if policy
                .high_impact_predicates
                .iter()
                .any(|term| term == edge.predicate())
            {
                Some("high_impact_predicate")
            } else if pairs.contains(&(edge.object_key.as_str(), edge.subject_key.as_str()))
                || targets
                    .get(&(edge.subject_key.as_str(), edge.predicate()))
                    .is_some_and(|items| items.len() > 1)
            {
                Some("conflicting_declaration")
            } else if !policy
                .routine_predicates
                .iter()
                .any(|term| term == edge.predicate())
            {
                Some("predicate_requires_review")
            } else {
                None
            }
        })
        .collect();
    let mut grouped = BTreeMap::<String, Vec<Routed>>::new();
    for (edge, reason) in edges.into_iter().zip(reasons) {
        let rows = grouped.entry(edge.chunk).or_default();
        ensure!(
            rows.len() < MAX_PER_CHUNK,
            "A passage exceeds the limit of 16 declared relationships."
        );
        rows.push(Routed {
            value: edge.value,
            reason,
            path: edge.path,
            start: edge.start,
            end: edge.end,
        });
    }
    let chunks: BTreeMap<_, _> = grouped
        .into_iter()
        .map(|(id, rows)| {
            let proof: Vec<_> = rows
                .iter()
                .map(|row| {
                    json!([
                        row.path,
                        row.start,
                        row.end,
                        row.value,
                        row.reason.unwrap_or(ROUTINE_RULE),
                    ])
                })
                .collect();
            let digest = files::digest(json!(proof).to_string().as_bytes());
            (id, DeclarationChunk { rows, digest })
        })
        .collect();
    let proof: Vec<_> = chunks
        .iter()
        .map(|(id, chunk)| (id, &chunk.digest))
        .collect();
    let digest = files::digest(&serde_json::to_vec(&(policy.digest()?, proof))?);
    Ok(Catalog { chunks, digest })
}

impl Catalog {
    pub(crate) fn load(
        db: &Connection,
        root: &Path,
        policy: &PublicationPolicy,
        predicates: &[String],
    ) -> Result<Self> {
        policy.validate(predicates)?;
        route(load_edges(db, root, predicates)?, policy)
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    pub(crate) fn chunk_ids(&self) -> impl Iterator<Item = &str> {
        self.chunks.keys().map(String::as_str)
    }

    pub(crate) fn chunk_digest(&self, chunk_id: &str) -> Option<&str> {
        self.chunks.get(chunk_id).map(|chunk| chunk.digest.as_str())
    }

    pub(crate) fn items(&self, chunk_id: &str) -> Vec<SharedItem> {
        self.chunks
            .get(chunk_id)
            .into_iter()
            .flat_map(|chunk| &chunk.rows)
            .map(|row| SharedItem {
                value: row.value.clone(),
                disposition: match row.reason {
                    Some(reason) => SharedDisposition::Review { reason },
                    None => SharedDisposition::Automatic {
                        rule: ROUTINE_RULE.to_owned(),
                    },
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocabulary() -> Vec<String> {
        ["depends_on", "owned_by", "uses"]
            .map(str::to_owned)
            .to_vec()
    }

    fn policy() -> PublicationPolicy {
        PublicationPolicy {
            trusted_paths: vec!["catalog/declared.md".into()],
            routine_predicates: vec!["depends_on".into(), "uses".into()],
            high_impact_predicates: vec!["owned_by".into()],
            single_valued_predicates: vec!["uses".into()],
        }
    }

    #[test]
    fn validates_exact_collection_paths_and_configured_vocabulary() {
        let mut policy = policy();
        assert!(policy.validate(&vocabulary()).is_ok());
        let original = policy.digest().unwrap();
        policy.routine_predicates.reverse();
        assert_eq!(policy.digest().unwrap(), original);
        for path in [
            "/catalog/declared.md",
            "catalog//declared.md",
            "catalog/../declared.md",
            "catalog\\declared.md",
            "C:/declared.md",
            "catalog/declared.pdf",
        ] {
            policy.trusted_paths = vec![path.into()];
            assert!(policy.validate(&vocabulary()).is_err(), "{path}");
        }
        policy.trusted_paths.clear();
        policy.high_impact_predicates.push("uses".into());
        assert!(policy.validate(&vocabulary()).is_err());
    }

    #[test]
    fn accepts_exact_unicode_rows_and_blocks_entire_malformed_fence() {
        let source = concat!(
            "Préface\r\n```anyrag-relations\r\nÅ Gate | depends_on | Β Relay\r\n```\r\n",
            "```anyrag-relations\nA Gate | uses | B Relay\nmalformed\nC Gate | uses | D Relay\n```\n",
            "```anyrag-relations\nE Gate | uses | F Relay\n",
        );
        let rows = parse(source, &vocabulary(), 10).unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows[0].valid_block);
        assert!(rows[1..].iter().all(|row| !row.valid_block));
        for row in rows {
            let quote = source
                .chars()
                .skip(row.start)
                .take(row.end - row.start)
                .collect::<String>();
            assert_eq!(row.value["quote"], quote);
        }
    }

    #[test]
    fn ignores_decoys_in_code_fences_comments_and_near_miss_headers() {
        let source = concat!(
            "````md\n```anyrag-relations\nA Gate | uses | B Relay\n```\n````\n",
            "<!--\n```anyrag-relations\nC Gate | uses | D Relay\n```\n-->\n",
            "```anyrag-relations \nE Gate | uses | F Relay\n```\n",
            "```anyrag-relations\nG Gate | uses | H Relay\n```\n",
        );
        let rows = parse(source, &vocabulary(), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value["subject"], "G Gate");
        assert!(rows[0].valid_block);
    }

    fn edge(chunk: &str, path: &str, text: &str) -> Edge {
        let row = declaration(text, 0, &vocabulary()).unwrap();
        Edge {
            chunk: chunk.into(),
            path: path.into(),
            start: row.start,
            end: row.end,
            subject_key: entity_key(row.value["subject"].as_str().unwrap()),
            object_key: entity_key(row.value["object"].as_str().unwrap()),
            value: row.value,
            valid_block: true,
        }
    }

    #[test]
    fn routes_conflicts_and_model_only_claims_without_a_confidence_rule() {
        let trusted = "catalog/declared.md";
        let catalog = route(
            vec![
                edge("1", trusted, "A Gate | depends_on | B Relay"),
                edge("2", "elsewhere.md", "b relay | uses | a gate"),
                edge("3", trusted, "M Gate | uses | N Relay"),
                edge("4", trusted, "M Gate | uses | O Relay"),
                edge("5", trusted, "P Gate | owned_by | Q Service"),
                edge("6", trusted, "R Gate | depends_on | S Relay"),
            ],
            &policy(),
        )
        .unwrap();
        for (id, reason) in [
            ("1", "conflicting_declaration"),
            ("2", "untrusted_declaration_path"),
            ("3", "conflicting_declaration"),
            ("4", "conflicting_declaration"),
            ("5", "high_impact_predicate"),
        ] {
            assert!(matches!(catalog.items(id)[0].disposition,
                             SharedDisposition::Review { reason: found } if found == reason));
        }
        assert!(
            matches!(&catalog.items("6")[0].disposition, SharedDisposition::Automatic { rule } if rule == ROUTINE_RULE)
        );
        let ordinary = json!({"subject":"Other Gate","predicate":"uses","object":"Other Relay","quote":"Other Gate uses Other Relay."});
        let high_impact = json!({"subject":"Other Gate","predicate":"owned_by","object":"Other Service","quote":"Other Gate is owned by Other Service."});
        let rows = policy().stage_proposals(vec![ordinary, high_impact]);
        assert!(matches!(
            rows[0].disposition,
            SharedDisposition::Review {
                reason: "model_only"
            }
        ));
        assert!(matches!(
            rows[1].disposition,
            SharedDisposition::Review {
                reason: "high_impact_predicate"
            }
        ));
        assert_eq!(catalog.chunk_ids().count(), 6);
        assert!(catalog.chunk_digest("6").is_some());
        assert!(catalog.items("missing").is_empty());
    }

    #[test]
    fn selects_first_unique_chunk_and_refuses_overfull_declared_passages() {
        let row = declaration("A Gate | uses | B Relay", 2, &vocabulary()).unwrap();
        let text = row.value["quote"].as_str().unwrap();
        let chunks = [
            Chunk {
                id: "earlier".into(),
                start: 0,
                end: 2 + text.len(),
                text: format!("xx{text}"),
            },
            Chunk {
                id: "later".into(),
                start: 2,
                end: 2 + text.len(),
                text: text.into(),
            },
        ];
        assert_eq!(owner(&row, &chunks), Some("earlier"));
        let many = (0..=MAX_PER_CHUNK)
            .map(|_| edge("same", "catalog/declared.md", text))
            .collect();
        assert!(route(many, &policy()).is_err());
    }

    #[test]
    fn binds_disk_revision_and_changes_only_affected_chunk_digests() {
        let folder = tempfile::tempdir().unwrap();
        let root = files::directory(folder.path()).unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sources(source_id TEXT, path TEXT, source_hash TEXT);
             CREATE TABLE chunks(row_id INTEGER PRIMARY KEY, chunk_id TEXT, source_id TEXT, start INTEGER, end INTEGER);
             CREATE VIRTUAL TABLE passages USING fts5(text, tokenize='porter unicode61');",
        )
        .unwrap();
        let index = |name: &str, row: &str| {
            let document = format!("Préface.\n\n{OPENER}\n{row}\n{CLOSER}\n");
            let source = root.join(name);
            std::fs::write(&source, &document).unwrap();
            db.execute(
                "INSERT INTO sources VALUES(?1,?2,?3)",
                rusqlite::params![
                    name,
                    files::display_path(&source).unwrap(),
                    files::digest(document.as_bytes())
                ],
            )
            .unwrap();
            let (start, end, text) = files::chunks(&document).next().unwrap();
            db.execute(
                "INSERT INTO chunks(chunk_id,source_id,start,end) VALUES(?1,?1,?2,?3)",
                rusqlite::params![name, start as i64, end as i64],
            )
            .unwrap();
            db.execute("INSERT INTO passages VALUES(?1)", [text])
                .unwrap();
        };
        index("a.md", "A Gate | depends_on | B Relay");
        index("c.md", "C Gate | depends_on | D Relay");
        let mut policy = policy();
        policy.trusted_paths = vec!["a.md".into(), "c.md".into()];
        let initial = Catalog::load(&db, &root, &policy, &vocabulary()).unwrap();
        index("b.md", "B Relay | uses | A Gate");
        let changed = Catalog::load(&db, &root, &policy, &vocabulary()).unwrap();
        assert_ne!(initial.digest(), changed.digest());
        assert_ne!(initial.chunk_digest("a.md"), changed.chunk_digest("a.md"));
        assert_eq!(initial.chunk_digest("c.md"), changed.chunk_digest("c.md"));
        std::fs::write(root.join("b.md"), "Unindexed source change.").unwrap();
        assert!(Catalog::load(&db, &root, &policy, &vocabulary()).is_err());
    }
}
