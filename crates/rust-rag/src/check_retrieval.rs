use crate::{files, store};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_CASES: usize = 100;
const MAX_TARGETS: usize = 16;

fn field<'a>(item: &'a Value, key: &str) -> Result<&'a str> {
    item[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .with_context(|| format!("Provide a nonempty {key} string."))
}

fn target(citation: &Value) -> Result<(&Path, &str)> {
    ensure!(
        citation.as_object().is_some_and(|fields| fields.len() == 2),
        "Provide path and quote for each target."
    );
    let path = field(citation, "path")?;
    ensure!(
        !path.contains(['\\', ':'])
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Use an exact source-relative path with '/' separators."
    );
    Ok((Path::new(path), field(citation, "quote")?))
}

fn check_case(
    db: &mut Connection,
    root: &Path,
    case: &Value,
    limit: usize,
    graph: bool,
) -> Result<(Vec<Option<usize>>, usize, Value)> {
    ensure!(
        case.as_object().is_some_and(|fields| fields.len() == 2),
        "Provide question and expected fields."
    );
    let question = field(case, "question")?;
    let expected = case["expected"]
        .as_array()
        .filter(|entries| entries.len() <= MAX_TARGETS)
        .with_context(|| format!("Provide an array of up to {MAX_TARGETS} expected targets."))?;
    let targets = expected.iter().map(target).collect::<Result<Vec<_>>>()?;
    let result = store::query(db, question, limit, graph, store::Mode::Lexical, None)?;
    let evidence = result["evidence"]
        .as_array()
        .context("Query returned no evidence list.")?;
    let mut ranks = vec![None; targets.len()];

    for (index, hit) in evidence.iter().enumerate() {
        let path = Path::new(field(hit, "path")?);
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if !targets
            .iter()
            .zip(&ranks)
            .any(|((path, _), rank)| *path == relative && rank.is_none())
        {
            continue;
        }
        let reopened = store::open_citation(db, field(hit, "chunk_id")?, field(hit, "revision")?)?;
        let passage = &reopened["evidence"];
        let text = field(passage, "text")?;
        if passage["path"] != hit["path"] {
            continue;
        }
        for ((path, quote), rank) in targets.iter().zip(&mut ranks) {
            if *path == relative && rank.is_none() && text.contains(quote) {
                *rank = Some(index + 1);
            }
        }
    }

    let coverage = &result["graph_coverage"];
    let graph_coverage = if graph {
        json!({
            "partial": coverage["partial"],
            "unavailable": coverage["unavailable"]
        })
    } else {
        Value::Null
    };
    Ok((ranks, evidence.len(), graph_coverage))
}

pub(crate) fn run(db: &mut Connection, input: &Path, limit: usize, graph: bool) -> Result<Value> {
    let index = store::status(db)?;
    let root = index["root"]
        .as_str()
        .context("Ingest a source folder first.")?;
    let root = Path::new(root);
    ensure!(root.is_absolute(), "The indexed source root is invalid.");
    let mut remaining = MAX_FILE_BYTES;
    let bytes = files::read(input, &mut remaining)
        .context("Read a regular local JSONL case file of at most 1 MiB.")?;
    let input = std::str::from_utf8(&bytes).context("Use UTF-8 in the JSONL case file.")?;
    let mut cases = Vec::new();
    let mut passed = 0;
    let mut targets_matched = 0;
    let mut targets_total = 0;

    for (index, text) in input.lines().enumerate() {
        let line = index + 1;
        ensure!(
            text.len() <= MAX_LINE_BYTES,
            "Line {line} exceeds the 16 KiB limit."
        );
        if text.trim().is_empty() {
            continue;
        }
        ensure!(cases.len() < MAX_CASES, "Use at most {MAX_CASES} cases.");
        let case: Value =
            serde_json::from_str(text).with_context(|| format!("Invalid JSON on line {line}."))?;
        let (ranks, retrieved, graph_coverage) = check_case(db, root, &case, limit, graph)
            .map_err(|error| anyhow::anyhow!("Line {line}: {error}"))?;
        let matched = ranks.iter().flatten().count();
        let complete = if ranks.is_empty() {
            retrieved == 0
        } else {
            matched == ranks.len()
        };
        passed += usize::from(complete);
        targets_matched += matched;
        targets_total += ranks.len();
        cases.push(json!({
            "line": line, "retrieved": retrieved, "passed": complete,
            "matched_ranks": ranks, "graph_coverage": graph_coverage
        }));
    }
    ensure!(!cases.is_empty(), "Provide at least one retrieval case.");
    Ok(json!({
        "schema_version": 1,
        "status": if passed == cases.len() { "passed" } else { "misses" },
        "mode": "lexical", "graph": graph, "limit": limit, "answer_generated": false,
        "cases_passed": passed, "cases_total": cases.len(),
        "targets_matched": targets_matched, "targets_total": targets_total, "cases": cases
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn checks_only_expected_active_sources_and_reports_no_input_text() -> Result<()> {
        let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let scratch = std::env::temp_dir().canonicalize()?.join(format!(
            "rust-rag-check-retrieval-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&scratch)?;
        let root = scratch.join("sources");
        let state = scratch.join("state");
        fs::create_dir(&root)?;
        fs::create_dir(&state)?;
        fs::create_dir(root.join("current"))?;
        fs::create_dir(root.join("archive"))?;
        let root = root.canonicalize()?;
        let input = scratch.join("questions.jsonl");
        let first = "CedarPump requires RelayKit.";
        let second = "RelayKit requires SpareBracket.";
        fs::write(root.join("pump.md"), first)?;
        fs::write(root.join("current/relay.md"), second)?;
        fs::write(
            root.join("archive/relay.md"),
            format!("Old mail copied: {second}"),
        )?;
        let mut db = store::open(&state, true)?;
        store::upgrade(&mut db, &state)?;
        store::ingest(&mut db, &root, false, false, false)?;
        let tx = db.transaction()?;
        graph::configure(&tx, "check-retrieval-test")?;
        for id in graph::pending(&tx, "check-retrieval-test", 8)? {
            let text = graph::unfinished_text(&tx, &id, "check-retrieval-test")?;
            if let Some((subject, object, quote)) = [
                ("CedarPump", "RelayKit", first),
                ("RelayKit", "SpareBracket", second),
            ]
            .into_iter()
            .find(|(_, _, quote)| text.as_deref() == Some(*quote))
            {
                assert!(graph::publish(
                    &tx,
                    &id,
                    "check-retrieval-test",
                    &["requires".to_owned()],
                    &[
                        json!({"subject": subject, "predicate": "requires", "object": object, "quote": quote})
                    ],
                )?);
            }
        }
        tx.commit()?;

        let positive = json!({
            "question": "CedarPump",
            "expected": [{"path": "pump.md", "quote": first}, {"path": "current/relay.md", "quote": second}]
        });
        let wrong_source = json!({
            "question": "RelayKit",
            "expected": [{"path": "pump.md", "quote": second}]
        });
        let revised = json!({
            "question": "RelayKit",
            "expected": [{"path": "current/relay.md", "quote": second}]
        });
        let expected_empty = json!({"question": "Outbuilding pressure", "expected": []});
        let unexpected_hit = json!({"question": "RelayKit", "expected": []});
        fs::write(
            &input,
            format!("{positive}\n{wrong_source}\n{revised}\n{expected_empty}\n{unexpected_hit}\n"),
        )?;
        let lexical = run(&mut db, &input, 5, false)?;
        assert_eq!(lexical["cases_passed"], 2);
        assert_eq!(lexical["cases"][0]["matched_ranks"], json!([1, null]));
        assert_eq!(lexical["cases"][1]["matched_ranks"], json!([null]));
        assert_eq!(lexical["cases"][3]["retrieved"], 0);
        assert_eq!(lexical["cases"][3]["passed"], true);
        assert!(lexical["cases"][4]["retrieved"].as_u64().unwrap() > 0);
        assert_eq!(lexical["cases"][4]["passed"], false);
        let graph = run(&mut db, &input, 5, true)?;
        assert_eq!(graph["cases_passed"], 3);
        assert!(graph["cases"][0]["matched_ranks"][1].as_u64().is_some());
        assert_eq!(graph["cases"][0]["graph_coverage"]["unavailable"], false);
        assert_eq!(graph["status"], "misses");
        let report = graph.to_string();
        for secret in [
            "CedarPump",
            "SpareBracket",
            "pump.md",
            "relay.md",
            "questions.jsonl",
        ] {
            assert!(!report.contains(secret), "report copied test input");
        }

        fs::write(
            root.join("current/relay.md"),
            "RelayKit requires a different bracket.",
        )?;
        store::ingest(&mut db, &root, true, false, false)?;
        let after = run(&mut db, &input, 5, false)?;
        assert_eq!(after["cases_passed"], 1);
        assert_eq!(after["cases"][2]["matched_ranks"], json!([null]));
        assert!(after["cases"][2]["retrieved"].as_u64().unwrap() > 0);

        fs::write(&input, json!({"question": "private", "expected": [{"path": "../outside.md", "quote": "private"}]}).to_string())?;
        let error = run(&mut db, &input, 5, false).unwrap_err().to_string();
        assert!(error.contains("Line 1"));
        assert!(!error.contains("outside.md"));
        assert!(!error.contains("private"));
        drop(db);
        fs::remove_dir_all(scratch)?;
        Ok(())
    }
}
