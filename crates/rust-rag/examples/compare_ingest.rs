//! Compare two standalone text builds on identical synthetic files.
//! Usage: cargo run --locked --release --example compare_ingest -- OLD NEW NEW_SCRATCH

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

const MIB: usize = 1_048_576;
const FILES: usize = 144;
const RUNS: usize = 4;
const TERM: &str = "Beaconmarker";

fn cli(binary: &Path, state: &Path, arguments: &[&str]) -> Result<Value> {
    let output = Command::new(binary)
        .args(["--state-dir"])
        .arg(state)
        .arg("--json")
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "{}: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn prepare(folder: &Path) -> Result<()> {
    fs::create_dir(folder)?;
    let mut bytes = vec![b' '; MIB];
    for file in 0..FILES {
        let note = format!("Synthetic document {file:03}. {TERM} means signal confirmed.");
        bytes[..note.len()].copy_from_slice(note.as_bytes());
        fs::write(folder.join(format!("{file:03}.md")), &bytes)?;
    }
    Ok(())
}

fn measure(binary: &Path, state: &Path, folder: &Path, steps: &mut Vec<Value>) -> Result<()> {
    let began = Instant::now();
    let ingest = cli(
        binary,
        state,
        &["ingest", folder.to_str().context("UTF-8 scratch path")?],
    )?;
    let millis = began.elapsed().as_millis();
    let status = cli(binary, state, &["status"])?;
    let sources = status["sources"].as_u64().context("source count")?;
    ensure!(
        sources == status["chunks"],
        "Expected one chunk per fixture."
    );
    let tail = cli(binary, state, &["query", "143", "--limit", "1"])?;
    let last_cited = tail["evidence"][0]["text"]
        == format!("Synthetic document 143. {TERM} means signal confirmed.");
    steps.push(json!({
        "invocation": steps.len() + 1, "sources": sources, "ingest_ms": millis,
        "bytes_read_reported": ingest["bytes_read"], "tail_cited": last_cited,
    }));
    Ok(())
}

fn lexical_capacity(old: &Path, new: &Path, scratch: &Path) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for count in [100, 1_000, 5_000] {
        let folder = scratch.join(format!("lexical-{count}"));
        fs::create_dir(&folder)?;
        for id in 0..count {
            let text = format!("Marker{id:04} is a synthetic catalog entry.");
            fs::write(folder.join(format!("{id:04}.md")), text)?;
        }
        let old_state = scratch.join(format!("lexical-before-{count}"));
        let new_state = scratch.join(format!("lexical-after-{count}"));
        let builds = [(old, &old_state), (new, &new_state)];
        let mut ingest_ms = Vec::new();
        for (binary, state) in builds {
            cli(binary, state, &["init"])?;
            let began = Instant::now();
            let report = cli(
                binary,
                state,
                &["ingest", folder.to_str().context("UTF-8 path")?],
            )?;
            ingest_ms.push(began.elapsed().as_millis());
            ensure!(
                report["updated_sources"] == count,
                "The corpus did not fully ingest."
            );
        }
        let mut timings = [Vec::new(), Vec::new()];
        for sample in 0..33 {
            let term = format!("Marker{:04}", [0, count / 2, count - 1][sample % 3]);
            let mut matches = Vec::new();
            for (position, (binary, state)) in builds.iter().enumerate() {
                let began = Instant::now();
                let report = cli(binary, state, &["query", &term, "--limit", "1"])?;
                if sample >= 3 {
                    timings[position].push(began.elapsed().as_secs_f64() * 1_000.0);
                }
                let hit = &report["evidence"][0];
                ensure!(
                    hit["text"] == format!("{term} is a synthetic catalog entry."),
                    "Wrong citation."
                );
                matches.push(hit["chunk_id"].clone());
            }
            ensure!(matches[0] == matches[1], "Citation identities differed.");
        }
        let rows: Vec<_> = timings
            .iter_mut()
            .zip(ingest_ms)
            .map(|(times, millis)| {
                times.sort_by(f64::total_cmp);
                json!({"ingest_ms": millis, "query_p50_ms": (times[14] + times[15]) / 2.0,
                "query_p95_ms": times[28]})
            })
            .collect();
        result.push(json!({"sources_and_chunks": count, "queries_per_build": 30,
            "baseline": rows[0], "candidate": rows[1]}));
    }
    Ok(result)
}

fn main() -> Result<()> {
    let [old, new, scratch]: [PathBuf; 3] = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .context("Supply OLD NEW NEW_SCRATCH.")?;
    fs::create_dir(&scratch).context("Choose a new scratch directory outside sync folders.")?;
    let scratch = fs::canonicalize(scratch)?;
    let folder = scratch.join("synthetic notes");
    prepare(&folder)?;
    let old_state = scratch.join("before");
    let new_state = scratch.join("after");
    let builds = [(&old, &old_state), (&new, &new_state)];
    for (binary, state) in builds {
        cli(binary, state, &["init"])?;
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    for _ in 0..RUNS {
        measure(&old, &old_state, &folder, &mut before)?;
        measure(&new, &new_state, &folder, &mut after)?;
    }
    ensure!(
        after
            .last()
            .is_some_and(|step| step["tail_cited"] == true && step["sources"] == FILES),
        "The candidate did not converge."
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "corpus_bytes": FILES * MIB, "source_files": FILES,
            "runs_per_build": RUNS, "baseline": before, "candidate": after,
            "lexical_capacity": lexical_capacity(&old, &new, &scratch)?,
        }))?
    );
    Ok(())
}
