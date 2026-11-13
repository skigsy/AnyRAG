"""Check the copied Rust executable against synthetic evidence and lifecycle fixtures."""

from __future__ import annotations

import argparse
import codecs
from contextlib import closing
from hashlib import sha256
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
from tempfile import TemporaryDirectory
import time
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[2]
MAX_DOCUMENT_BYTES = 16 * 1024 * 1024


def invoke(command, state, cwd, env, *args, fail=False):
    result = subprocess.run(
        [*command, "--state-dir", str(state), *map(str, args), "--json"],
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        encoding="utf-8",
        timeout=60,
    )
    assert (result.returncode != 0) == fail, (args, result.returncode, result.stdout, result.stderr)
    return json.loads(result.stdout)


def check_result(result, status):
    assert result["schema_version"] == 1
    assert result["id_scheme"] == "rust-rag-v1"
    assert result["status"] == status, result
    assert result["mode"] == "lexical"
    assert result["graph"] is False and result["graph_coverage"] is None
    assert result["evidence_role"] == "candidate_context"
    assert result["answer_support"] == "unverified"
    assert result["answer_generated"] is False
    if status == "abstained":
        assert result["evidence"] == [] and result["abstention_reason"]
    else:
        assert result["evidence"] and result["abstention_reason"] is None
        assert all(hit["match_status"] == status for hit in result["evidence"])
        assert all(hit["answer_support"] == "unverified" for hit in result["evidence"])


def check_evidence(rust, vault):
    forms = (
        ("BOM UTF8.Md", "syntheticlilac", codecs.BOM_UTF8, "utf-8"),
        ("Little endian.txt", "syntheticcedar", codecs.BOM_UTF16_LE, "utf-16-le"),
        ("Big endian.md", "syntheticmaple", codecs.BOM_UTF16_BE, "utf-16-be"),
    )
    for directory in (".obsidian", ".Git", ".trash"):
        excluded = vault / directory
        excluded.mkdir()
        (excluded / "ignored.md").write_text("syntheticexcluded", encoding="utf-8")
    documents = []
    for name, term, bom, encoding in forms:
        # The first and later chunks exercise leading trim, overlap, and code-point offsets.
        decoded = "\x1c\t🧭 Cafe\u0301 heading\r\n" + "scratch " * 160 + f"\r\n{term}\r\n\x1f"
        raw = bom + decoded.encode(encoding)
        source = vault / name
        source.write_bytes(raw)
        documents.append((source, term, decoded, raw))

    assert rust("ingest", vault)["updated_sources"] == len(documents)
    repeated = rust("ingest", vault)
    assert repeated["updated_sources"] == 0 and repeated["unchanged_sources"] == len(documents)
    for source, term, decoded, raw in documents:
        actual = rust("query", term)
        check_result(actual, "evidence")
        for hit in actual["evidence"]:
            assert Path(hit["path"]) == source
            assert hit["source_hash"] == sha256(raw).hexdigest()
            assert hit["text"] == decoded[hit["start"]:hit["end"]]
            assert hit["start"] > 1_000 and hit["page"] is None

    check_result(rust("query", "heading", "--limit", 3), "evidence")
    check_result(rust("query", "syntheticlilac absentone absenttwo"), "weak_match")
    for absent in ("the and who", "syntheticexcluded", "absentone"):
        check_result(rust("query", absent), "abstained")
    actual = rust("query", "heading", "--limit", 3)
    assert {Path(hit["path"]) for hit in actual["evidence"]} == {d[0] for d in documents}
    assert all(hit["start"] == 2 and hit["text"].startswith("🧭 Cafe\u0301") for hit in actual["evidence"])
    return [source for source, _, _, _ in documents]


def check_lifecycle(rust, vault, state, sources):
    revised, invalid, oversized = sources
    baseline = rust("query", "syntheticlilac")
    explained = rust("query", "syntheticlilac", "--explain")
    explanation = explained.pop("explanation")
    assert explained == baseline
    original = baseline["evidence"][0]
    assert explanation["results"][0]["chunk_id"] == original["chunk_id"]
    assert explanation["results"][0]["signals"] == ["lexical"]

    def citation(hit, *, fail=False):
        return rust("citation", hit["chunk_id"], "--revision", hit["revision"], fail=fail)

    opened = citation(original)
    assert opened["status"] == "active"
    for key in ("source_id", "revision", "chunk_id", "text", "path", "start", "end"):
        assert opened["evidence"][key] == original[key]
    canary = b"syntheticretirementcanaryfiftyeight"
    original_bytes = revised.read_bytes()
    replacement_bytes = canary + b" is stored in the fictional blue cabinet."
    revised.write_bytes(replacement_bytes)
    assert rust("ingest", vault)["updated_sources"] == 1
    check_result(rust("query", "syntheticlilac"), "abstained")
    assert "evidence" not in citation(original, fail=True)
    replacement = rust("query", canary.decode())["evidence"][0]
    assert replacement["source_id"] == original["source_id"]
    assert replacement["chunk_id"] != original["chunk_id"]
    revised.write_bytes(original_bytes)
    assert rust("ingest", vault)["updated_sources"] == 1
    assert citation(original) == opened
    revised.write_bytes(replacement_bytes)
    assert rust("ingest", vault)["updated_sources"] == 1

    invalid.write_bytes(b"\xff invalid UTF-8")
    with oversized.open("wb") as stream:
        stream.truncate(MAX_DOCUMENT_BYTES + 1)
    (vault / "unsupported.csv").write_text("item,count", encoding="utf-8")
    review = rust("ingest", vault, "--review")
    assert len(review["failures"]) == 2
    rows = {row["path"]: row for row in review["source_review"]}
    assert rows[invalid.name]["status"] == "failed" and rows[invalid.name]["retained_previous"]
    assert rows["unsupported.csv"]["status"] == "skipped" and rows["unsupported.csv"]["reason"]
    check_result(rust("query", "syntheticcedar"), "evidence")
    check_result(rust("query", "syntheticmaple"), "evidence")

    missing = vault / "Removed from vault.md"
    missing.write_text("syntheticmissingsource", encoding="utf-8")
    assert rust("ingest", vault)["updated_sources"] == 1
    missing.unlink()
    assert rust("ingest", vault)["removed_sources"] == 0
    check_result(rust("query", "syntheticmissingsource"), "evidence")

    outside = vault.parent / "synthetic external target"
    outside.mkdir()
    (outside / "hidden.md").write_text("syntheticoutsidevault", encoding="utf-8")
    link = vault / "directory link"
    if os.name == "nt":
        import _winapi

        _winapi.CreateJunction(str(outside), str(link))
    else:
        link.symlink_to(outside, target_is_directory=True)
    try:
        blocked = rust("ingest", vault, "--sync", "--review")
        assert blocked["sync_blocked"] and blocked["removed_sources"] == 0
        assert all(row["status"] != "removed" for row in blocked["source_review"])
        assert blocked["skipped_links_or_placeholders"] == 1
        check_result(rust("query", "syntheticoutsidevault"), "abstained")
        check_result(rust("query", "syntheticmissingsource"), "evidence")
        assert rust("ingest", link, fail=True)["status"] == "error"
    finally:
        if os.name == "nt":
            link.rmdir()
        else:
            link.unlink()

    reconciled = rust("ingest", vault, "--sync", "--review")
    assert not reconciled["sync_blocked"] and reconciled["removed_sources"] == 1
    assert [row["path"] for row in reconciled["source_review"] if row["status"] == "removed"] == [missing.name]
    assert len(reconciled["failures"]) == 2
    check_result(rust("query", "syntheticmissingsource"), "abstained")
    check_result(rust("query", "syntheticcedar"), "evidence")
    check_result(rust("query", "syntheticmaple"), "evidence")

    database = state / "personal.sqlite3"
    assert canary in database.read_bytes()
    raw = revised.read_bytes()
    removed = rust("remove", original["source_id"])
    assert removed["removed_sources"] == 1 and removed["logical_removal"]
    assert not removed["source_files_changed"] and revised.read_bytes() == raw
    assert removed["purge"]["secure_delete"] and removed["purge"]["fts_secure_delete"]
    check_result(rust("query", canary.decode()), "abstained")
    assert "evidence" not in citation(replacement, fail=True)
    assert canary not in database.read_bytes()
    assert not any(state.glob("personal.sqlite3-*"))
    assert rust("remove", "--all")["removed_sources"] == 2
    counts = rust("status")
    assert counts["sources"] == counts["chunks"] == 0
    assert all(source.is_file() for source in sources)


def check_retrieval_edges(rust, work):
    folder, state = (work / name for name in ("language-fixtures", "language-index"))
    folder.mkdir()
    (folder / "unicode.md").write_text("🧭 Cafe\u0301 冷蔵手順 uses shelf seven.", encoding="utf-8")
    (folder / "first.md").write_text("C++ uses Beacon's first route.", encoding="utf-8")
    (folder / "second.md").write_text("C# uses Beacon's second route.", encoding="utf-8")

    def run(*args):
        return rust(*args, index=state)

    run("init")
    assert run("ingest", folder)["updated_sources"] == 3
    for query in ("Cafe\u0301", "冷蔵手順"):
        result = run("query", query)
        assert result["status"] == "evidence"
        [hit] = result["evidence"]
        assert hit["text"] == "🧭 Cafe\u0301 冷蔵手順 uses shelf seven."
        assert hit["start"] == 0 and hit["end"] == len(hit["text"])
    # The default FTS tokenizer treats punctuation as a boundary. The
    # candidate set must be inspected when a symbol carries meaning.
    for query in ("C++", "C#"):
        hits = run("query", query)["evidence"]
        assert {Path(hit["path"]).name for hit in hits} == {"first.md", "second.md"}
    check_result(run("query", "冷蔵手順 unrelated other"), "weak_match")
    check_result(run("query", "inventedabsentterm"), "abstained")


def check_interrupted_ingest(binary, runtime, env, work):
    vault = work / "interruption-vault"
    vault.mkdir()
    state = work / "interruption-state"
    command = [str(binary), "--state-dir", str(state)]

    def run(*args):
        return invoke([str(binary)], state, runtime, env, *args)

    source = vault / "00 original.md"
    source.write_text("syntheticoriginalbeforeinterruption", encoding="utf-8")
    run("init")
    run("ingest", vault)
    previous = run("query", "syntheticoriginalbeforeinterruption")["evidence"][0]
    source.write_text("syntheticupdatedduringinterruption", encoding="utf-8")
    candidates = 8
    padding = b"syntheticpartialindexcandidate".ljust(65 * 1024)
    for index in range(candidates):
        (vault / f"10 candidate {index:03}.md").write_bytes(padding)
    database = state / "personal.sqlite3"
    journal = state / "personal.sqlite3-journal"
    # A fresh inventory root forces the first ingestion transaction to write.
    with closing(sqlite3.connect(database)) as writer, writer:
        writer.execute("UPDATE settings SET root = NULL WHERE id = 1")
    # Hold a SQLite read transaction so the writer can create its rollback
    # journal but cannot commit before we observe and terminate it.
    with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as reader:
        reader.execute("BEGIN")
        reader.execute("SELECT root FROM settings WHERE id = 1").fetchone()
        with subprocess.Popen(
            [*command, "ingest", str(vault), "--json"],
            cwd=runtime,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ) as process:
            try:
                deadline = time.monotonic() + 3
                while not journal.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.005)
                interrupted = journal.exists() and process.poll() is None
            finally:
                if process.poll() is None:
                    process.kill()
                stdout, stderr = process.communicate(timeout=10)
            assert interrupted, ("Ingest did not reach the interruption point.", stdout, stderr)
            assert process.returncode != 0, (stdout, stderr)

    # The interrupted write must preserve the original anchored revision.
    [hit] = run("query", "syntheticoriginalbeforeinterruption")["evidence"]
    assert Path(hit["path"]) == source
    for field in ("source_id", "chunk_id", "source_hash", "start", "end", "text"):
        assert hit[field] == previous[field]
    check_result(run("query", "syntheticupdatedduringinterruption"), "abstained")
    assert run("status")["sources"] == 1

    resumed = run("ingest", vault)
    assert not resumed["failures"] and not resumed["byte_budget_exhausted"]
    assert run("status")["sources"] == candidates + 1
    check_result(run("query", "syntheticoriginalbeforeinterruption"), "abstained")
    [hit] = run("query", "syntheticupdatedduringinterruption")["evidence"]
    assert hit["source_id"] == previous["source_id"] and hit["chunk_id"] != previous["chunk_id"]


def check_sync_state_guard(rust, work):
    sync_folder = work / "OneDrive - Synthetic"
    sync_folder.mkdir()
    names = [sync_folder]
    if os.name == "nt":
        import ctypes as c

        kernel = c.WinDLL("kernel32", winmode=0x800)  # LOAD_LIBRARY_SEARCH_SYSTEM32
        short_path = c.create_unicode_buffer(32_768)
        size = kernel.GetShortPathNameW(str(sync_folder), short_path, len(short_path))
        assert 0 < size < len(short_path)
        alias = Path(short_path.value)
        if alias.name != sync_folder.name:
            names.append(alias)

    for name in names:
        blocked = rust("init", fail=True, index=name / "index")
        assert "cloud sync folders" in blocked["message"], blocked
    assert not (sync_folder / "index").exists()


def check_cloud_placeholder(rust, work):
    """Create an offline Cloud Files placeholder with no cloud account or provider connection."""
    import ctypes as c

    # ctypes declarations match the Windows SDK's cfapi.h and winbase.h.
    class Registration(c.Structure):
        _fields_ = [
            ("size", c.c_uint32), ("provider", c.c_wchar_p), ("version", c.c_wchar_p),
            ("sync_identity", c.c_void_p), ("sync_length", c.c_uint32),
            ("file_identity", c.c_void_p), ("file_length", c.c_uint32),
            ("provider_id", c.c_uint32 * 4),
        ]

    class Policies(c.Structure):
        _fields_ = [
            ("size", c.c_uint32), ("hydration_primary", c.c_uint16),
            ("hydration_modifiers", c.c_uint16), ("population_primary", c.c_uint16),
            ("population_modifiers", c.c_uint16), ("in_sync", c.c_uint32),
            ("hard_links", c.c_uint32), ("management", c.c_uint32),
        ]

    class FileBasicInfo(c.Structure):
        _fields_ = [("timestamps", c.c_int64 * 4), ("attributes", c.c_uint32)]

    class FileMetadata(c.Structure):
        _fields_ = [("basic", FileBasicInfo), ("file_size", c.c_int64)]

    class Placeholder(c.Structure):
        _fields_ = [
            ("name", c.c_wchar_p), ("metadata", FileMetadata), ("identity", c.c_void_p),
            ("identity_length", c.c_uint32), ("flags", c.c_uint32), ("result", c.c_long),
            ("usn", c.c_int64),
        ]

    api = c.WinDLL(str(Path(os.environ["SystemRoot"]) / "System32" / "CldApi.dll"))
    api.CfRegisterSyncRoot.argtypes = (c.c_wchar_p, c.POINTER(Registration), c.POINTER(Policies), c.c_uint32)
    api.CfCreatePlaceholders.argtypes = (c.c_wchar_p, c.POINTER(Placeholder), c.c_uint32, c.c_uint32, c.POINTER(c.c_uint32))
    api.CfUnregisterSyncRoot.argtypes = (c.c_wchar_p,)

    def require_ok(operation, result):
        assert result == 0, f"{operation} failed: HRESULT {result & 0xffffffff:#010x}"

    sync_root = work / "synthetic-cloud-root"
    vault = sync_root / "vault"
    vault.mkdir(parents=True)
    index = work / "cloud-state"
    local_note = vault / "local.md"
    local_note.write_text("syntheticcloudsyncguard", encoding="utf-8")
    rust("init", index=index)
    assert rust("ingest", vault, index=index)["updated_sources"] == 1
    local_note.unlink()

    cloud_note = "synthetic-placeholder.md"
    identity = c.create_string_buffer(b"synthetic-cloud-test")
    placeholder = Placeholder(
        cloud_note, FileMetadata(FileBasicInfo(attributes=0x80), file_size=1024),
        c.cast(identity, c.c_void_p), len(identity),
    )
    registration = Registration(c.sizeof(Registration), "Rust RAG synthetic test", "1")
    # FULL hydration and FULL population allow a dehydrated file under an ordinary local folder.
    policies = Policies(c.sizeof(Policies), hydration_primary=2, population_primary=2)
    disable_on_demand_population_on_root = 2
    require_ok("CfRegisterSyncRoot", api.CfRegisterSyncRoot(
        str(sync_root), c.byref(registration), c.byref(policies), disable_on_demand_population_on_root,
    ))
    try:
        processed = c.c_uint32()
        require_ok("CfCreatePlaceholders", api.CfCreatePlaceholders(
            str(vault), c.byref(placeholder), 1, 0, c.byref(processed),
        ))
        assert processed.value == 1
        require_ok("placeholder creation", placeholder.result)

        def offline_attributes():
            with os.scandir(vault) as entries:
                stat = next(entry for entry in entries if entry.name == cloud_note).stat(follow_symlinks=False)
            assert stat.st_reparse_tag == 0x9000001a  # IO_REPARSE_TAG_CLOUD
            assert stat.st_file_attributes & 0x00400000  # FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            return stat.st_file_attributes

        before = offline_attributes()
        result = rust("ingest", vault, "--sync", index=index)
        assert result["skipped_links_or_placeholders"] == 1 and not result["failures"], result
        assert result["sync_blocked"] and result["removed_sources"] == 0, result
        check_result(rust("query", "syntheticcloudsyncguard", index=index), "evidence")
        assert offline_attributes() == before
    finally:
        require_ok("CfUnregisterSyncRoot", api.CfUnregisterSyncRoot(str(sync_root)))
    print("PASS: synthetic Windows Cloud Files placeholder stayed offline and blocked sync removals")


def check_advanced(rust, work):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    from threading import Thread

    vault = work / "advanced-vault"
    state = work / "advanced-state"
    vault.mkdir()

    def run(*command, fail=False):
        return rust(*command, index=state, fail=fail)

    seed = vault / "pump.md"
    neighbor = vault / "relay.md"
    race = vault / "race.md"
    seed.write_text("CedarPump depends on RelayKit.", encoding="utf-8")
    neighbor.write_text("RelayKit requires SpareBracket.", encoding="utf-8")
    race.write_text("RaceValve uses OldFitting.", encoding="utf-8")
    pdf = vault / "manual.pdf"
    shutil.copyfile(ROOT / "examples/library/sample.pdf", pdf)
    assert run("init")["storage_version"] == 1
    assert run("ingest", vault)["updated_sources"] == 3
    original = run("query", "CedarPump")["evidence"][0]
    assert run("query", "CedarPump", "--graph", fail=True)["status"] == "error"
    upgraded = run("init", "--upgrade")
    assert upgraded["storage_version"] == 3
    with closing(sqlite3.connect(Path(upgraded["upgrade_backup"]).as_uri() + "?mode=ro", uri=True)) as backup:
        assert backup.execute("PRAGMA user_version").fetchone() == (1,)
    assert run("query", "CedarPump")["evidence"][0] == original
    assert "upgrade_backup" not in run("init", "--upgrade")
    assert run("ingest", vault, "--pdf")["updated_sources"] == 1
    manual = run("query", "cabinet nine")["evidence"][0]
    assert manual["page"] == 1 and "stored in cabinet nine" in manual["text"]
    assert manual["end"] - manual["start"] == len(manual["text"])
    assert manual["source_hash"] == sha256(pdf.read_bytes()).hexdigest()
    pdf.write_bytes(b"%PDF-1.4\nSynthetic malformed PDF\n")
    assert len(run("ingest", vault, "--pdf", "--sync")["failures"]) == 1
    assert run("query", "cabinet nine")["evidence"][0] == manual
    assert run("ingest", vault, "--sync")["removed_sources"] == 0
    assert run("query", "cabinet nine")["evidence"][0] == manual

    fixture_edges = {
        "CedarPump depends on RelayKit.": ("CedarPump", "depends_on", "RelayKit"),
        "RelayKit requires SpareBracket.": ("RelayKit", "requires", "SpareBracket"),
        "RaceValve uses OldFitting.": ("RaceValve", "uses", "OldFitting"),
    }
    errors = []
    embedding_inputs = []
    redirected = []

    class Model(BaseHTTPRequestHandler):
        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path == "/redirect/chat/completions":
                    self.send_response(307)
                    self.send_header("Location", f"http://127.0.0.1:{self.server.server_port}/target")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.path == "/target":
                    redirected.append(True)
                    self.send_error(500)
                    return
                if self.path == "/v1/embeddings":
                    assert set(body) == {"input", "model", "encoding_format"}
                    input_ = body["input"]
                    assert input_.startswith(("q: ", "p: "))
                    embedding_inputs.append(input_)
                    if input_ == "p: RaceValve is retired.":
                        race.write_text("RaceValve is finalized.", encoding="utf-8")
                        assert run("ingest", vault)["updated_sources"] == 1
                    values = [1, 0, 0] if "SpareBracket" in input_ or input_.startswith("q: ") else [0, 1, 0]
                    if input_ == "q: invalid vector":
                        values = [1, 0]
                    response = {"data": [{"index": 0, "embedding": values}]}
                else:
                    request = json.loads(body["messages"][-1]["content"])
                    response = {"choices": [{"message": {"content": json.dumps(self.chat(request))}}]}
                data = json.dumps(response).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            except Exception as exc:
                errors.append(exc)
                self.send_error(500)

        def chat(self, request):
            if "passage" in request:
                assert set(request) == {"passage", "predicates"}
                passage = request["passage"]
                if passage == "RaceValve uses OldFitting.":
                    race.write_text("RaceValve is retired.", encoding="utf-8")
                    assert run("ingest", vault)["updated_sources"] == 1
                if passage in fixture_edges:
                    subject, predicate, object_ = fixture_edges[passage]
                    return {"assertions": [
                        {"subject": subject, "predicate": predicate, "object": object_, "quote": passage}
                    ]}
                return {"assertions": []}
            assert set(request) == {"question", "evidence"}
            assert all(set(hit) == {"id", "passage"} for hit in request["evidence"])
            ids = [hit["id"] for hit in request["evidence"] if "SpareBracket" in hit["passage"]]
            return {"status": "answered", "answer": "RelayKit requires SpareBracket.",
                    "citations": ["fabricated"] if "fabricated" in request["question"] else ids}

        def log_message(self, *_):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Model) as server:
        thread = Thread(target=server.serve_forever)
        thread.start()
        try:
            endpoint = work / "synthetic-model.json"
            endpoint.write_text(json.dumps({
                "url": f"http://127.0.0.1:{server.server_port}/v1/chat/completions",
                "model": "synthetic", "timeout_seconds": 10,
            }), encoding="utf-8")
            first = run("extract-graph", "--model-endpoint", endpoint)
            assert first["completed_chunks"] == 3 and first["stale_chunks"] == 1, first
            assert first["pending"] and not first["failures"]
            second = run("extract-graph", "--model-endpoint", endpoint)
            assert second["completed_chunks"] == 1 and not second["pending"], second
            baseline = run("query", "CedarPump")
            assert all("SpareBracket" not in hit["text"] for hit in baseline["evidence"])
            graph = run("query", "CedarPump", "--graph")
            assert graph["graph_coverage"]["seed_status"] == "seeded"
            long_question = run("query", "How should we determine whether CedarPump ownership procedures affect regulatory escalation?", "--graph")
            assert long_question["graph_coverage"]["seed_status"] == "no_strong_seed"
            [related] = [hit for hit in graph["evidence"] if "SpareBracket" in hit["text"]]
            assert related["graph_support"]["assertion"]["quote"] == "RelayKit requires SpareBracket."
            assert set(related["graph_support"]["supporting_chunk_ids"]) <= {
                hit["chunk_id"] for hit in graph["evidence"]
            }
            assert all("OldFitting" not in hit["text"] for hit in run("query", "RaceValve", "--graph")["evidence"])
            answer = run("answer", "CedarPump", "--graph", "--model-endpoint", endpoint)
            assert answer["status"] == "answered" and answer["citations"] == [related["chunk_id"]]
            assert answer["answer_support"] == "unverified"
            assert run("answer", "CedarPump fabricated", "--graph", "--model-endpoint",
                       endpoint, fail=True)["status"] == "error"

            embeddings = work / "synthetic-embedding.json"
            spec = {"url": f"http://127.0.0.1:{server.server_port}/v1/embeddings",
                    "model": "synthetic", "dimensions": 3,
                    "query_prefix": "q: ", "passage_prefix": "p: "}
            embeddings.write_text(json.dumps(spec), encoding="utf-8")
            first = run("embed", "--embedding-endpoint", embeddings)
            assert first["completed_chunks"] == 3 and first["stale_chunks"] == 1 and first["pending"], first
            partial = run("query", "replacement component", "--mode", "vector", "--embedding-endpoint", embeddings)
            assert partial["vector_coverage"] == {"indexed_chunks": 3, "pending_chunks": 1, "partial": True}
            second = run("embed", "--embedding-endpoint", embeddings)
            assert second["completed_chunks"] == 1 and not second["pending"], second
            semantic = run("query", "replacement component", "--mode", "vector", "--embedding-endpoint", embeddings)
            assert semantic["status"] == "weak_match" and semantic["mode"] == "vector"
            [vector_hit] = semantic["evidence"]
            assert vector_hit["chunk_id"] == related["chunk_id"] and vector_hit["score"] == 1.0
            hybrid = run("query", "CedarPump", "--mode", "hybrid", "--embedding-endpoint", embeddings)
            assert hybrid["status"] == "evidence" and hybrid["mode"] == "hybrid"
            assert {hit["chunk_id"] for hit in hybrid["evidence"]} == {original["chunk_id"], related["chunk_id"]}
            assert run("query", "invalid vector", "--mode", "vector", "--embedding-endpoint",
                       embeddings, fail=True)["status"] == "error"
            attempts = len(embedding_inputs)
            embeddings.write_text(json.dumps({**spec, "model": "different"}), encoding="utf-8")
            assert run("query", "replacement component", "--mode", "vector", "--embedding-endpoint",
                       embeddings, fail=True)["status"] == "error"
            assert len(embedding_inputs) == attempts
            embeddings.write_text(json.dumps(spec), encoding="utf-8")
            removed = run("remove", related["source_id"])
            assert removed["removed_sources"] == 1
            assert removed["purge"]["retained_rollback_copies"] == 1
            assert removed["purge"]["rollback_may_retain_evidence"]
            with closing(sqlite3.connect(Path(upgraded["upgrade_backup"]).as_uri() + "?mode=ro", uri=True)) as backup:
                assert backup.execute("SELECT count(*) FROM passages WHERE passages MATCH 'SpareBracket'").fetchone() == (1,)
            assert run("discard-rollback", fail=True)["status"] == "error"
            assert run("discard-rollback", "--confirm")["discarded_copies"] == 1
            assert not Path(upgraded["upgrade_backup"]).exists()
            assert neighbor.is_file()
            assert all("SpareBracket" not in hit["text"] for hit in run("query", "CedarPump", "--graph")["evidence"])
            after = run("query", "replacement component", "--mode", "vector", "--embedding-endpoint", embeddings)
            assert after["status"] == "abstained" and after["evidence"] == []
            endpoint.write_text(json.dumps({
                "url": f"http://127.0.0.1:{server.server_port}/redirect/chat/completions",
                "model": "synthetic", "timeout_seconds": 10,
            }), encoding="utf-8")
            assert run("answer", "CedarPump", "--model-endpoint", endpoint, fail=True)["status"] == "error"
            assert not redirected
            assert not errors, errors
        finally:
            server.shutdown()
            thread.join()

    connection = sqlite3.connect(state / "personal.sqlite3")
    try:
        connection.execute("PRAGMA user_version=99")
    finally:
        connection.close()
    assert run("status", fail=True)["status"] == "error"
    print("PASS: native PDF, explicit upgrade, graph, vector and hybrid retrieval, and cited answers")


def check_docx(rust, work):
    state = work / "word-state"
    folder = work / "word"
    folder.mkdir()
    source = folder / "letter.docx"
    types = (
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Override PartName="/word/document.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
        '</Types>'
    )
    relationships = (
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="r1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"'
        ' Target="word/document.xml"/></Relationships>'
    )
    first = "Cafe\u0301 🧭"
    quote = "A table beacon gives coordinates."

    def save(body):
        with ZipFile(source, "w", ZIP_DEFLATED) as archive:
            archive.writestr("[Content_Types].xml", types)
            archive.writestr("_rels/.rels", relationships)
            archive.writestr("word/document.xml",
                             '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
                             f'<w:body>{body}</w:body></w:document>')

    def run(*args):
        return rust(*args, index=state)

    save(f"<w:p><w:r><w:t>{first}</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p>"
         f"<w:ins><w:r><w:t>{quote}</w:t></w:r></w:ins>"
         "<w:r><w:rPr><w:vanish/></w:rPr><w:t>hiddenwordcanary</w:t></w:r>"
         "</w:p></w:tc></w:tr></w:tbl>")
    run("init", "--upgrade")
    assert run("ingest", folder, "--docx")["updated_sources"] == 1
    [hit] = run("query", "coordinates")["evidence"]
    assert hit["text"] == quote and hit["source_hash"] == sha256(source.read_bytes()).hexdigest()
    assert hit["anchor"] == "table:1:row:1:cell:1:p:1"
    assert hit["start"] == len(first) + 2 and hit["end"] == len(first) + 2 + len(quote)
    assert run("query", "hiddenwordcanary")["evidence"] == []
    source.write_bytes(b"invalid package")
    report = run("ingest", folder, "--docx")
    assert report["failures"][0]["retained_previous"] is True
    assert run("query", "coordinates")["evidence"][0]["chunk_id"] == hit["chunk_id"]
    save("")
    assert run("ingest", folder, "--docx")["empty_sources"] == 1
    assert run("query", "coordinates")["evidence"] == []
    assert quote.encode() not in (state / "personal.sqlite3").read_bytes()
    assert run("remove", hit["source_id"])["removed_sources"] == 1 and source.exists()
    print("PASS: native Word worker, Unicode/table anchor, corrupt replacement, blank and purge")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path, help="Built rust-rag executable")
    parser.add_argument("--check-cloud-placeholder", action="store_true", help="Also create a temporary Windows Cloud Files placeholder")
    parser.add_argument("--check-advanced", action="store_true", help="Verify the native pdf and model feature build")
    args = parser.parse_args()
    if not __debug__:
        parser.error("Run with Python assertions enabled.")
    if args.check_cloud_placeholder and os.name != "nt":
        parser.error("The Cloud Files fixture requires Windows.")
    binary = args.executable.resolve(strict=True)
    with TemporaryDirectory(prefix="rust-rag-check-") as scratch:
        work = Path(scratch).resolve()
        runtime = work / "runtime"
        runtime.mkdir()
        copied = runtime / ("rust-rag.exe" if os.name == "nt" else "rust-rag")
        shutil.copy2(binary, copied)
        rust_env = {"PATH": os.defpath}
        if os.name == "nt":
            windows = Path(os.environ["SystemRoot"])
            rust_env = {
                "SystemRoot": str(windows),
                "PATH": os.pathsep.join(map(str, (windows / "System32", windows))),
                "TEMP": str(work),
                "TMP": str(work),
            }
        state, baseline_state, vault = (work / name for name in ("rust-state", "python-state", "vault"))
        vault.mkdir()

        def run_rust(*command, fail=False, index=state):
            return invoke([str(copied)], index, runtime, rust_env, *command, fail=fail)

        assert run_rust("init")["sources"] == 0
        doctor = run_rust("doctor")
        assert doctor["status"] == "ready" and doctor["sqlite"]["fts5"]
        assert doctor["sqlite"]["secure_delete"] and doctor["sqlite"]["fts_secure_delete"]
        assert doctor["sqlite"]["journal_mode"] == "delete"
        assert Path(run_rust("status")["state_dir"]) == state
        sources = check_evidence(run_rust, vault)
        check_retrieval_edges(run_rust, work)
        assert run_rust("init")["sources"] == len(sources)
        export_path = work / "evidence.jsonl"
        exported = run_rust("export", "--output", export_path)
        assert exported["exported_passages"] == run_rust("status")["chunks"]
        assert "syntheticlilac" not in json.dumps(exported)
        snapshot = export_path.read_bytes()
        assert b"syntheticlilac" in snapshot
        assert run_rust("export", "--output", export_path, fail=True)["status"] == "error"
        assert export_path.read_bytes() == snapshot
        baseline_index = baseline_state / "index.sqlite3"
        baseline_state.mkdir(mode=0o700)
        baseline_index.write_bytes(b"synthetic legacy state")
        baseline_bytes = baseline_index.read_bytes()
        assert run_rust("init", fail=True, index=baseline_state)["status"] == "error"
        assert baseline_index.read_bytes() == baseline_bytes
        assert not (baseline_state / "personal.sqlite3").exists() and not (state / "index.sqlite3").exists()

        foreign_state = work / "unrelated-state"
        foreign_state.mkdir(mode=0o700)
        unrelated = foreign_state / "personal.sqlite3"
        connection = sqlite3.connect(unrelated)
        try:
            connection.execute("CREATE TABLE unrelated(id INTEGER PRIMARY KEY)")
        finally:
            connection.close()
        unrelated_bytes = unrelated.read_bytes()
        assert run_rust("init", fail=True, index=foreign_state)["status"] == "error"
        assert unrelated.read_bytes() == unrelated_bytes
        check_lifecycle(run_rust, vault, state, sources)
        check_interrupted_ingest(copied, runtime, rust_env, work)
        check_sync_state_guard(run_rust, work)
        if args.check_cloud_placeholder:
            check_cloud_placeholder(run_rust, work)
        if args.check_advanced:
            check_advanced(run_rust, work)
            if run_rust("capabilities")["compiled"]["docx"]:
                check_docx(run_rust, work)
            assert run_rust("demo")["path"] == ["CedarPump", "RelayKit", "SpareBracket"]
        print(f"PASS: copied binary, citation integrity, lifecycle and interruption (SQLite {doctor['sqlite']['version']})")


if __name__ == "__main__":
    main()
