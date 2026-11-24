//! Resolve a single row and column in a parser-qualified native table.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_SOURCES: usize = 100;
const MAX_CELLS: usize = 4_096;
const MAX_LABEL_BYTES: usize = 256;

pub(crate) struct Support {
    pub(crate) chunk_id: String,
    pub(crate) anchor: String,
}

pub(crate) struct CellMatch {
    pub(crate) chunk_id: String,
    pub(crate) header: Support,
    pub(crate) row: Support,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Table {
    Sheet(String),
    Word(u32),
}

struct Cell {
    chunk_id: String,
    anchor: String,
    text: String,
}

impl Cell {
    fn support(&self) -> Support {
        Support {
            chunk_id: self.chunk_id.clone(),
            anchor: self.anchor.clone(),
        }
    }
}

fn positive_decimal(value: &str) -> Option<u32> {
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn position(anchor: &str) -> Option<(Table, u32, u32)> {
    if let Some(sheet) = anchor.strip_prefix("sheet:") {
        let (name, address) = sheet.rsplit_once('!')?;
        if name.is_empty() || name.chars().any(char::is_control) {
            return None;
        }
        let split = address.find(|character: char| character.is_ascii_digit())?;
        let (letters, digits) = address.split_at(split);
        if !(1..=3).contains(&letters.len()) || !letters.bytes().all(|b| b.is_ascii_uppercase()) {
            return None;
        }
        let column = letters
            .bytes()
            .fold(0_u32, |value, byte| 26 * value + u32::from(byte - b'A' + 1));
        let row = positive_decimal(digits)?;
        return (column <= 16_384 && row <= 1_048_576).then(|| (Table::Sheet(name.to_owned()), row, column));
    }
    let parts = anchor.split(':').collect::<Vec<_>>();
    let ["table", table, "row", row, "cell", column, "p", "1"] = parts.as_slice() else {
        return None;
    };
    Some((
        Table::Word(positive_decimal(table)?),
        positive_decimal(row)?,
        positive_decimal(column)?,
    ))
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

struct Mention {
    first_end: usize,
    last_start: usize,
}

fn mention(question: &[String], text: &str) -> Option<Mention> {
    if text.len() > MAX_LABEL_BYTES {
        return None;
    }
    let label = words(text).collect::<Vec<_>>();
    if label.is_empty() || label.len() > question.len() {
        return None;
    }
    let mut matches = question
        .windows(label.len())
        .enumerate()
        .filter_map(|(start, phrase)| (phrase == label).then_some(start));
    let start = matches.next()?;
    Some(Mention {
        first_end: start + label.len(),
        last_start: matches.next_back().unwrap_or(start),
    })
}

pub(crate) fn find(
    db: &Connection,
    question: &str,
    evidence: &[Value],
) -> Result<Option<CellMatch>> {
    let sources = evidence
        .iter()
        .filter_map(|hit| hit["source_id"].as_str())
        .collect::<BTreeSet<_>>();
    let question = words(question).collect::<Vec<_>>();
    if sources.is_empty() || sources.len() > MAX_SOURCES || question.len() < 2 {
        return Ok(None);
    }
    let available: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
         WHERE type='table' AND name='native_table_cells')",
        [],
        |row| row.get(0),
    )?;
    if !available {
        return Ok(None);
    }
    let mut query = db.prepare(
        "SELECT c.chunk_id,c.anchor,p.text
         FROM chunks c JOIN native_table_cells n ON n.chunk_id=c.chunk_id
         JOIN passages p ON p.rowid=c.row_id
         WHERE c.source_id=?1 LIMIT ?2",
    )?;
    let mut seen = 0;
    let mut association = None;
    let mut matched_pair = false;
    for source in sources {
        let mut tables: BTreeMap<Table, BTreeMap<(u32, u32), Cell>> = BTreeMap::new();
        let remaining = i64::try_from(MAX_CELLS + 1 - seen)?;
        let mut rows = query.query((source, remaining))?;
        while let Some(row) = rows.next()? {
            seen += 1;
            if seen > MAX_CELLS {
                return Ok(None);
            }
            let Some(anchor) = row.get::<_, Option<String>>(1)? else {
                return Ok(None);
            };
            let cell = Cell {
                chunk_id: row.get(0)?,
                anchor,
                text: row.get(2)?,
            };
            let Some((table, row, column)) = position(&cell.anchor) else {
                return Ok(None);
            };
            if tables.entry(table).or_default().insert((row, column), cell).is_some() {
                return Ok(None);
            }
        }
        for cells in tables.values() {
            let mut headers = cells.iter().filter_map(|(&(row, column), cell)| {
                if row != 1 || column <= 1 {
                    return None;
                }
                mention(&question, &cell.text).map(|matched| (column, cell, matched))
            });
            let mut labels = cells.iter().filter_map(|(&(row, column), cell)| {
                if row <= 1 || column != 1 {
                    return None;
                }
                mention(&question, &cell.text).map(|matched| (row, cell, matched))
            });
            let (Some((column, header, hm)), Some((row, label, lm))) =
                (headers.next(), labels.next())
            else {
                continue;
            };
            if headers.next().is_some() || labels.next().is_some() {
                return Ok(None);
            }
            if hm.first_end > lm.last_start && lm.first_end > hm.last_start {
                // A single occurrence must not serve as both the header and row label.
                continue;
            }
            if matched_pair {
                return Ok(None);
            }
            matched_pair = true;
            association = cells.get(&(row, column)).and_then(|cell| {
                (!cell.text.trim().is_empty()).then(|| CellMatch {
                    chunk_id: cell.chunk_id.clone(),
                    header: header.support(),
                    row: label.support(),
                })
            });
        }
    }
    Ok(association)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_only_one_complete_active_table_and_keeps_precise_support_anchors() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE passages(text TEXT);
             CREATE TABLE chunks(chunk_id TEXT PRIMARY KEY,source_id TEXT,anchor TEXT,row_id INTEGER);
             CREATE TABLE native_table_cells(
                 chunk_id TEXT PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE
             ) WITHOUT ROWID;",
        )?;
        let insert = |source: &str, id: &str, anchor: &str, text: &str, eligible: bool| -> Result<()> {
            db.execute("INSERT INTO passages(text) VALUES(?1)", [text])?;
            db.execute(
                "INSERT INTO chunks VALUES(?1,?2,?3,?4)",
                (id, source, anchor, db.last_insert_rowid()),
            )?;
            if eligible {
                db.execute("INSERT INTO native_table_cells VALUES(?1)", [id])?;
            }
            Ok(())
        };
        for (id, anchor, text) in [
            ("h", "sheet:Operations!B1", "Inspection interval"),
            ("r", "sheet:Operations!A2", "Cedar pump"),
            ("v", "sheet:Operations!B2", "Every 18 days"),
        ] {
            insert("grid", id, anchor, text, true)?;
        }
        for (id, anchor, text) in [
            ("wh", "table:1:row:1:cell:2:p:1", "Inspection interval"),
            ("wr", "table:1:row:2:cell:1:p:1", "Cedar pump"),
            ("wv", "table:1:row:2:cell:2:p:1", "Every 23 days"),
        ] {
            insert("word", id, anchor, text, true)?;
        }
        insert("grid", "false", "sheet:Operations!B2", "Unmarked data", false)?;

        let grid = json!({"source_id":"grid"});
        let word = json!({"source_id":"word"});
        let question = "What is the inspection interval for Cedar pump?";
        let hit = find(&db, question, &[grid.clone()])?.unwrap();
        assert_eq!(hit.chunk_id, "v");
        assert_eq!(hit.header.chunk_id, "h");
        assert_eq!(hit.header.anchor, "sheet:Operations!B1");
        assert_eq!(hit.row.chunk_id, "r");
        assert_eq!(hit.row.anchor, "sheet:Operations!A2");
        let hit = find(&db, question, &[word.clone()])?.unwrap();
        assert_eq!(hit.chunk_id, "wv");
        assert_eq!(hit.header.anchor, "table:1:row:1:cell:2:p:1");
        assert_eq!(hit.row.anchor, "table:1:row:2:cell:1:p:1");
        assert!(find(&db, question, &[word, grid.clone()])?.is_none());
        assert!(find(&db, "Inspection interval for Cedar pumps?", &[grid.clone()])?.is_none());
        assert!(find(&db, "Inspection due interval for Cedar pump?", &[grid.clone()])?.is_none());
        db.execute("DELETE FROM chunks WHERE chunk_id='v'", [])?;
        assert!(find(&db, question, &[grid.clone()])?.is_none());
        insert("grid", "wrong", "sheet:Operations!B02", "Every 18 days", true)?;
        assert!(find(&db, question, &[grid])?.is_none());
        assert!(position("table:1:row:2:cell:2:p:2").is_none());
        assert!(position("sheet:Operations!XFE2").is_none());
        assert!(position("sheet:Operations!A1048577").is_none());
        db.execute("DROP TABLE native_table_cells", [])?;
        assert!(find(&db, question, &[json!({"source_id":"word"})])?.is_none());
        Ok(())
    }
}
