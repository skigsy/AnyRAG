use crate::{files, store};
use anyhow::{Context, Result, ensure};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NOTE_BYTES: usize = 256 * 1024;
const MAX_VISITED: usize = 32;
const MAX_LINKS: usize = 256;
const MAX_LINKS_CHECKED: usize = 2_048;

struct Note {
    path: PathBuf,
    source_id: String,
    revision: String,
}

fn note_key(value: &str) -> &str {
    match value.get(value.len().saturating_sub(3)..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".md") => &value[..value.len() - 3],
        _ => value,
    }
}

fn resolve<'a>(notes: &'a BTreeMap<String, Note>, name: &str) -> Option<&'a str> {
    let name = note_key(name.trim());
    if name.contains(['\\', ':', '^', '%', '[', ']'])
        || name.chars().any(char::is_control)
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    let mut matches = notes.keys().map(String::as_str).filter(|path| {
        let key = note_key(path);
        if name.contains('/') {
            key == name
        } else {
            key.rsplit('/').next() == Some(name)
        }
    });
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
}

fn current_text(note: &Note, bytes_left: &mut usize) -> Option<String> {
    let cap = (*bytes_left).min(MAX_NOTE_BYTES);
    let mut remaining = cap;
    let bytes = files::read(&note.path, &mut remaining);
    *bytes_left -= cap - remaining;
    let bytes = bytes.ok()?;
    if files::digest(&bytes) != note.revision {
        return None;
    }
    files::decode(&bytes).ok()
}

pub(crate) fn run(db: &mut Connection, start: &str, depth: u8) -> Result<Value> {
    let tx = db.transaction()?;
    let root = store::status(&tx)?["root"]
        .as_str()
        .map(PathBuf::from)
        .context("Ingest a Markdown vault first.")?;
    let notes = {
        let mut statement = tx.prepare(
            "SELECT path,source_id,source_hash FROM sources WHERE lower(substr(path,-3))='.md' ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Note {
                path: PathBuf::from(row.get::<_, String>(0)?),
                source_id: row.get(1)?,
                revision: row.get(2)?,
            })
        })?;
        let mut notes = BTreeMap::new();
        for note in rows {
            let note = note?;
            let Ok(relative) = note.path.strip_prefix(&root) else {
                continue;
            };
            let Some(relative) = relative.to_str() else {
                continue;
            };
            #[cfg(windows)]
            let relative = relative.replace('\\', "/");
            #[cfg(not(windows))]
            let relative = relative.to_owned();
            notes.insert(relative, note);
        }
        notes
    };
    let start = resolve(&notes, start)
        .context("Use one indexed Markdown path or an unambiguous note name.")?;
    let mut queue = VecDeque::from([(start, 0)]);
    let mut visited = BTreeSet::from([start]);
    let mut availability = BTreeMap::new();
    let mut bytes_left = MAX_BYTES;
    let mut links = Vec::new();
    let mut checked = 0;
    let mut unresolved = 0;
    let mut unavailable = 0;
    let mut unanchored = 0;
    let mut partial = false;
    let mut anchor = tx.prepare(
        "SELECT chunk_id,start,end FROM chunks WHERE source_id=?1 AND start<=?2 AND end>=?3
         ORDER BY start DESC,chunk_id LIMIT 1",
    )?;

    'notes: while let Some((path, hop)) = queue.pop_front() {
        let note = &notes[path];
        let Some(text) = current_text(note, &mut bytes_left) else {
            ensure!(
                hop != 0,
                "The starting note is unavailable or changed. Sync it before checking links."
            );
            unavailable += 1;
            continue;
        };
        availability.insert(path, true);
        let options = Options::ENABLE_WIKILINKS | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
        let mut commented = false;
        let mut in_link = false;
        let mut previous_byte = 0;
        let mut previous_char = 0;
        for (event, span) in Parser::new_ext(&text, options).into_offset_iter() {
            match event {
                Event::Text(piece) if !in_link => {
                    if piece.matches("%%").count() % 2 == 1 {
                        commented = !commented;
                    }
                }
                Event::End(TagEnd::Link | TagEnd::Image) => in_link = false,
                Event::Start(Tag::Image { .. }) => in_link = true,
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }) => {
                    in_link = true;
                    if commented || !matches!(link_type, LinkType::WikiLink { .. }) {
                        continue;
                    }
                    if checked == MAX_LINKS_CHECKED {
                        partial = true;
                        break 'notes;
                    }
                    checked += 1;
                    let Some(quote) = text.get(span.clone()) else {
                        continue;
                    };
                    if !quote.starts_with("[[")
                        || !quote.ends_with("]]")
                        || quote.contains(['\r', '\n'])
                    {
                        continue;
                    }
                    let target_name = dest_url.split('#').next().unwrap_or_default();
                    let Some(target) = resolve(&notes, target_name) else {
                        unresolved += 1;
                        continue;
                    };
                    if target == path {
                        continue;
                    }
                    if !*availability
                        .entry(target)
                        .or_insert_with(|| current_text(&notes[target], &mut bytes_left).is_some())
                    {
                        unavailable += 1;
                        continue;
                    }
                    let Some(between) = text.get(previous_byte..span.start) else {
                        continue;
                    };
                    previous_char += between.chars().count();
                    previous_byte = span.start;
                    let from = previous_char;
                    let to = from + quote.chars().count();
                    let found = anchor
                        .query_row((&note.source_id, from as i64, to as i64), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        })
                        .optional()?;
                    let Some((chunk, chunk_start, chunk_end)) = found else {
                        unanchored += 1;
                        continue;
                    };
                    if links.len() >= MAX_LINKS {
                        partial = true;
                        break 'notes;
                    }
                    links.push(json!({
                        "from": path, "to": target, "hop": hop + 1,
                        "evidence": {"chunk_id": chunk,"revision":note.revision, "start":from,"end":to,
                                     "chunk_start":chunk_start,"chunk_end":chunk_end,"quote":quote}
                    }));
                    if hop + 1 < depth && !visited.contains(target) {
                        if visited.len() >= MAX_VISITED {
                            partial = true;
                        } else {
                            visited.insert(target);
                            queue.push_back((target, hop + 1));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(json!({
        "schema_version":1,"start":start,"depth":depth,"links":links,
        "unresolved_links":unresolved,"unavailable_notes":unavailable,"unanchored_links":unanchored,
        "partial":partial || bytes_left == 0 || unresolved > 0 || unavailable > 0 || unanchored > 0,
    }))
}

#[cfg(all(test, feature = "model"))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reopens_a_literal_link_and_withholds_a_moved_target() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let sources = scratch.path().join("notes");
        let state = scratch.path().join("state");
        fs::create_dir(&sources)?;
        let sources = files::directory(&sources)?;
        let state = files::state_directory(&state, true)?;
        let mut index = store::open(&state, true)?;
        let entry = sources.join("Entry.md");
        let relay = sources.join("Relay.md");
        fs::write(
            &entry,
            "# Étape\n\nCheck [[Relay|the part]]. `[[Decoy]]` and %% [[Decoy]] %%.",
        )?;
        fs::write(&relay, "Read [[Entry]] before proceeding.")?;
        fs::write(sources.join("Decoy.md"), "This is not linked from prose.")?;
        store::ingest(&mut index, &sources, true, false, false)?;

        let found = run(&mut index, "Entry", 2)?;
        let links = found["links"]
            .as_array()
            .context("Expected link entries.")?;
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["to"], "Relay.md");
        let reference = &links[0]["evidence"];
        let reopened = store::open_citation(
            &mut index,
            reference["chunk_id"].as_str().unwrap(),
            reference["revision"].as_str().unwrap(),
        )?;
        let offset = reopened["evidence"]["start"].as_u64().unwrap();
        let start = reference["start"].as_u64().unwrap() - offset;
        let end = reference["end"].as_u64().unwrap() - offset;
        let quote: String = reopened["evidence"]["text"]
            .as_str()
            .unwrap()
            .chars()
            .skip(start as usize)
            .take((end - start) as usize)
            .collect();
        assert_eq!(quote, "[[Relay|the part]]");

        fs::rename(relay, sources.join("Replacement.md"))?;
        let stale = run(&mut index, "Entry", 2)?;
        assert_eq!(stale["links"], json!([]));
        assert_eq!(stale["unavailable_notes"], 1);
        Ok(())
    }
}
