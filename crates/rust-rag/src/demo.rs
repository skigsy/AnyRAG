use crate::{files, graph, store};
use anyhow::{Context, Result, ensure};
use rusqlite::TransactionBehavior::Immediate;
use serde_json::{Value, json};
use std::fs;

pub fn run() -> Result<Value> {
    const EXTRACTOR: &str = "public-graph-demo-v1";
    const FIXTURES: [(&str, &str, &str); 2] = [
        ("pump.md", "CedarPump", "RelayKit"),
        ("relay.md", "RelayKit", "SpareBracket"),
    ];
    let temp = tempfile::Builder::new()
        .prefix("rust-rag-demo-")
        .tempdir()?;
    let sources = temp.path().join("sources");
    let state = temp.path().join("index");
    fs::create_dir(&sources)?;
    let state = files::state_directory(&state, true)?;
    let mut db = store::open(&state, true)?;
    store::upgrade(&mut db, &state)?;
    for (filename, subject, object) in FIXTURES {
        fs::write(
            sources.join(filename),
            format!("{subject} requires {object}."),
        )?;
    }
    let sources = files::directory(&sources)?;
    let ingestion = store::ingest(&mut db, &sources, true, false, false)?;
    ensure!(ingestion["updated_sources"] == 2, "Demo indexing failed.");
    let tx = db.transaction_with_behavior(Immediate)?;
    graph::configure(&tx, EXTRACTOR)?;
    let predicates = ["requires".to_owned()];
    for id in graph::pending(&tx, EXTRACTOR, 8)? {
        let quote =
            graph::unfinished_text(&tx, &id, EXTRACTOR)?.context("A demo source disappeared.")?;
        let (_, subject, object) = FIXTURES
            .into_iter()
            .find(|(_, subject, object)| quote == format!("{subject} requires {object}."))
            .context("Unexpected demo source.")?;
        ensure!(
            graph::publish(
                &tx,
                &id,
                EXTRACTOR,
                &predicates,
                &[
                    json!({"subject": subject, "predicate": "requires", "object": object, "quote": quote})
                ]
            )?,
            "Demo graph publication failed."
        );
    }
    tx.commit()?;
    let lexical = store::query(&mut db, "CedarPump", 5, false, store::Mode::Lexical, None)?;
    let expanded = store::query(&mut db, "CedarPump", 5, true, store::Mode::Lexical, None)?;
    let baseline = lexical["evidence"]
        .as_array()
        .context("Missing lexical demo evidence.")?;
    let hits = expanded["evidence"]
        .as_array()
        .context("Missing graph demo evidence.")?;
    ensure!(
        baseline.len() == 1
            && hits.len() == 2
            && hits[1]["text"] == "RelayKit requires SpareBracket.",
        "Demo did not return the expected second hop."
    );
    let removed_id = hits[1]["source_id"]
        .as_str()
        .context("Missing demo source ID.")?;
    ensure!(
        store::remove(&mut db, Some(removed_id))? == 1,
        "Demo removal failed."
    );
    let after = store::query(&mut db, "CedarPump", 5, true, store::Mode::Lexical, None)?;
    ensure!(
        after["evidence"]
            .as_array()
            .is_some_and(|hits| hits.len() == 1),
        "Removed hop survived."
    );
    Ok(json!({
        "schema_version": 1,
        "question": "What does CedarPump depend on, directly or indirectly?",
        "search_seed": "CedarPump",
        "fixture_extractor": true,
        "model_used": false,
        "lexical_quotes": [baseline[0]["text"]],
        "graph_quotes": [hits[0]["text"], hits[1]["text"]],
        "path": ["CedarPump", "RelayKit", "SpareBracket"],
        "second_hop_after_removal": false,
        "source_files_changed": false
    }))
}
