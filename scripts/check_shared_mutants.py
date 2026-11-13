"""Run the shared rehearsal against a clean Git revision and four disposable Rust faults.

Requires the rehearsal prerequisites, Git, and Cargo with the locked dependencies cached.
Only the source-only scratch checkout is changed. The output directory receives receipts.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from rehearse_shared import sha


ROOT = Path(__file__).resolve().parents[1]
SHARED = "crates/rust-rag/src/shared.rs"
GRAPH = "crates/rust-rag/src/graph.rs"
CARGO_PATHS = (
    ".cargo/config.toml", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
    "crates/rust-rag/Cargo.toml", "crates/rust-rag/src",
)


def exact_replace(text, old, new):
    if text.count(old) != 1:
        raise ValueError("Fault transform did not match exactly once")
    return text.replace(old, new)


def stale_citation(text):
    old = '''                return Ok(json!({"schema_version":1,"evidence":item}));
            }
        }
        self.check_scope(principal, &scope, None)?;
        Err(NOT_FOUND)'''
    new = '''                let result = json!({"schema_version":1,"evidence":item});
                fixture_cache().lock().map_err(unavailable)?.insert(id.into(), result.clone());
                return Ok(result);
            }
        }
        self.check_scope(principal, &scope, None)?;
        if let Some(result) = fixture_cache().lock().map_err(unavailable)?.get(id)
            && result["evidence"]["collection"].as_str().is_some_and(|name| scope.iter().any(|s| s == name))
        {
            return Ok(result.clone());
        }
        return Err(NOT_FOUND);

        fn fixture_cache() -> &'static Mutex<BTreeMap<String, Value>> {
            static CACHE: std::sync::LazyLock<Mutex<BTreeMap<String, Value>>> =
                std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));
            &CACHE
        }'''
    return exact_replace(text, old, new)


def stale_graph(text):
    old = '''    expand_using(db, seeds, limit, None)
}'''
    new = '''    // Disposable fault: reuse graph evidence after its source revision changes.
    type Expansion = (Vec<Value>, Value);
    static CACHE: std::sync::Mutex<Option<(String, Expansion)>> = std::sync::Mutex::new(None);
    let key = seeds.first().and_then(|seed| seed["chunk_id"].as_str());
    let mut cache = CACHE.lock().expect("fixture graph cache");
    if let Some((cached_key, cached)) = cache.as_ref()
        && Some(cached_key.as_str()) == key
    {
        return Ok(cached.clone());
    }
    let expansion = expand_using(db, seeds, limit, None)?;
    if let Some(key) = key && cache.is_none() && !expansion.0.is_empty() {
        *cache = Some((key.into(), expansion.clone()));
    }
    Ok(expansion)
}'''
    # Earlier and later source revisions use a different expand_using signature.
    if old not in text:
        old = old.replace("db, seeds, limit", "db, seeds, question, limit")
        new = new.replace("db, seeds, limit", "db, seeds, question, limit")
    return exact_replace(text, old, new)


def omit_post_dispatch_check(text):
    return exact_replace(text, """        });
        self.check_scope(principal, &scope, answer_dest)?;
        if let Some(name) = embedding""", """        });
        // Disposable fault: trust the policy snapshot from before model dispatch.
        if let Some(name) = embedding""")


def reopen_early(text):
    return exact_replace(text, """        let (maintenance, reconciled, pending) = self.phase()?;
        if !maintenance || pending || reconciled.as_deref() != Some(&digest) {""",
                         """        let (maintenance, _reconciled, pending) = self.phase()?;
        if !maintenance || pending {""")


FAULTS = (
    ("disabled_revision_checks", SHARED, stale_citation, "edited_citation_retired"),
    ("disabled_graph_retirement", GRAPH, stale_graph, "stale_extraction_withheld"),
    ("disabled_revocation_rechecks", SHARED, omit_post_dispatch_check, "revoke_after_dispatch"),
    ("premature_restore_reopen", SHARED, reopen_early, "restore_closed"),
)


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, timeout=30)


def frozen_source(source_ref, destination):
    commit = git("rev-parse", "--verify", "--end-of-options", f"{source_ref}^{{commit}}").strip().decode()
    raw_manifest = git("show", f"{commit}:public-files.json")
    manifest = json.loads(raw_manifest)
    files = manifest["files"]
    if manifest["schema_version"] != 1 or not isinstance(files, list) or not all(isinstance(p, str) for p in files):
        raise ValueError("Invalid public source manifest")
    allowed = set(files)
    if len(allowed) != len(files):
        raise ValueError("Duplicate path in public source manifest")
    for item in git("ls-tree", "-rz", commit, "--", *CARGO_PATHS).split(b"\0"):
        if not item:
            continue
        meta, relative = item.decode().split("\t", 1)
        mode, kind, digest = meta.split()
        path = destination / relative
        if (relative not in allowed or mode not in ("100644", "100755") or kind != "blob"
                or not path.resolve().is_relative_to(destination)):
            raise ValueError("Unapproved file type or path in public source revision")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(git("cat-file", "blob", digest))
    return commit, sha(raw_manifest)


def run_oracle(binary, output, openssl):
    run = subprocess.run([sys.executable, "-B", str(ROOT / "scripts/rehearse_shared.py"),
                          "--binary", str(binary), "--openssl", openssl, "--receipt", str(output)],
                         check=False, timeout=120)
    if not output.is_file():
        raise RuntimeError("The rehearsal did not write a receipt")
    return run.returncode, json.loads(output.read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-ref", default="HEAD", help="tracked source revision to test")
    parser.add_argument("--openssl", default="openssl")
    parser.add_argument("--output-dir", type=Path, required=True, help="new directory for redacted receipts")
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir()
    with tempfile.TemporaryDirectory(prefix="rust-rag-mutants-") as temp:
        scratch = Path(temp).resolve()
        source = scratch / "src"
        target = scratch / "target"
        commit, manifest_digest = frozen_source(args.source_ref, source)
        binary = target / "debug" / ("rust-rag.exe" if os.name == "nt" else "rust-rag")

        def build():
            subprocess.run(["cargo", "build", "--locked", "--offline", "--features", "shared",
                            "--target-dir", str(target)], cwd=source, check=True, timeout=300)

        build()
        status, baseline = run_oracle(binary, output / "baseline.json", args.openssl)
        if status != 0 or baseline["status"] != "pass":
            raise RuntimeError("The clean baseline did not pass")
        results = []
        for name, relative, transform, check in FAULTS:
            path = source / relative
            original = path.read_bytes()
            patched = transform(original.decode()).encode()
            try:
                path.write_bytes(patched)
                build()
                status, receipt = run_oracle(binary, output / f"{name}.json", args.openssl)
            finally:
                path.write_bytes(original)
            earlier = [item for item in receipt["checks"] if item["status"] != "unexercised"]
            caught = (status == 1 and receipt["status"] == "fail" and receipt.get("failed_check") == check
                      and all(item["status"] == "pass" for item in earlier[:-1]))
            results.append({"fault": name, "first_expected_failure": check,
                            "first_observed_failure": receipt.get("failed_check"), "caught": caught,
                            "binary_sha256": receipt["binary_sha256"],
                            "clean_source_file_sha256": sha(original), "fault_source_file_sha256": sha(patched)})
        caught = sum(item["caught"] for item in results)
        summary = {"schema_version": 1, "method": "disposable_compiled_source_mutants",
                   "source_commit": commit, "source_allowlist_sha256": manifest_digest,
                   "baseline_binary_sha256": baseline["binary_sha256"], "faults": results}
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(f"Shared Graph RAG compiled faults. {caught}/{len(FAULTS)} failed at the expected check.")
    return 0 if caught == len(FAULTS) else 1


if __name__ == "__main__":
    raise SystemExit(main())
