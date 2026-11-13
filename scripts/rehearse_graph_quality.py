"""Reproduce the frozen mixed Graph RAG benchmark with local, pinned model assets."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import subprocess
import tempfile
from time import monotonic, perf_counter, sleep
from urllib.error import URLError
from urllib.request import ProxyHandler, build_opener


DEFAULT_FIXTURE = Path(__file__).resolve().parents[1] / "examples/graph-quality/fixture.json"
MANIFEST_SHA256 = "d734b2af5e105608719848b317c1b210b15d71fd9f773b0df2e50cf702416af3"
MODEL_SHA256 = "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5"
PROFILES = {
    "windows-vulkan": {
        "system": "Windows",
        "ort_sha256": "b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a",
        "server_sha256": "2b66ab6316cea760da83f971c9a8ab80cc8551630bd603a238cf84cff9ce8638",
        "gpu_layers": "99",
    },
    "linux-cpu": {
        "system": "Linux",
        "ort_sha256": "d132535d051344ff5c64c9c200004150559049a81ed330eb4422c1962fb6b7e4",
        "server_sha256": "3cbd231c167eb2e88ae523180d223a6bc50141de8232635dc174e20891c324f9",
        "gpu_layers": "0",
    },
}
endpoint = {"url": "http://127.0.0.1:17917/v1/chat/completions", "model": "graph-qualification-qwen",
            "response_format": "schema", "predicates": ["depends_on", "requires", "owned_by", "part_of", "uses"],
            "max_tokens": 512, "timeout_seconds": 30}
max_chunks = 100
max_passes = 2
max_extract_seconds = 480


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def load_fixture(path):
    with path.open("rb") as stream:
        data = stream.read(128 * 1024 + 1)
    if len(data) > 128 * 1024:
        raise ValueError("The compact fixture exceeds its size limit.")
    bundle = json.loads(data)
    if not isinstance(bundle, dict) or not isinstance(bundle.get("manifest"), str):
        raise ValueError("The compact fixture must contain the frozen manifest text.")
    manifest_bytes = bundle["manifest"].encode("utf-8")
    if hashlib.sha256(manifest_bytes).hexdigest() != MANIFEST_SHA256:
        raise ValueError("The frozen graph fixture manifest has changed.")
    manifest = json.loads(manifest_bytes)
    public = bundle.get("public")
    if not isinstance(public, dict) or set(public) != set(manifest["public"]):
        raise ValueError("The fixture inventory differs from the frozen manifest.")
    for name, content in public.items():
        if name != "questions.json" and re.fullmatch(r"corpus/[a-z0-9-]+\.md", name) is None:
            raise ValueError("Unsafe path in the graph fixture.")
        if not isinstance(content, str) or hashlib.sha256(content.encode("utf-8")).hexdigest() != manifest["public"][name]:
            raise ValueError(f"Fixture payload changed: {name}")
    return manifest, public


@contextmanager
def local_server(server, model, profile):
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 17917))
    arguments = [
        server, "--model", model, "--alias", endpoint["model"], "--host", "127.0.0.1",
        "--port", "17917", "--offline", "--reasoning", "off", "--ctx-size", "8192",
        "--parallel", "1", "--threads", "4", "--gpu-layers", profile["gpu_layers"],
    ]
    environment = os.environ.copy()
    if profile["system"] == "Linux":
        environment["LD_LIBRARY_PATH"] = str(server.parent)
    flags = subprocess.CREATE_NO_WINDOW if profile["system"] == "Windows" else 0
    worker = subprocess.Popen(arguments, cwd=server.parent, env=environment,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                              stdin=subprocess.DEVNULL, creationflags=flags)
    try:
        opener = build_opener(ProxyHandler({}))
        deadline = monotonic() + 120
        while monotonic() < deadline:
            if worker.poll() is not None:
                raise ValueError(f"The pinned llama-server exited ({worker.returncode}).")
            try:
                with opener.open("http://127.0.0.1:17917/health", timeout=1) as response:
                    if json.loads(response.read(4096)).get("status") == "ok":
                        break
            except (TimeoutError, URLError):
                pass
            sleep(0.25)
        else:
            raise ValueError("The pinned llama-server did not become ready in 120 seconds.")
        yield
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:
                worker.wait(timeout=10)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()


def write(path, result):
    pending = path.with_suffix(".pending")
    pending.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    pending.replace(path)


def command(binary, *args, timeout=30):
    start = perf_counter()
    try:
        completed = subprocess.run([binary, *map(str, args), "--json"], check=True, capture_output=True,
                                   encoding="utf-8", timeout=timeout)
        value = {"result": json.loads(completed.stdout)}
    except subprocess.TimeoutExpired:
        value = {"error": "timeout"}
    except subprocess.CalledProcessError as error:
        value = {"error": "command_failed", "exit_code": error.returncode, "message": error.stderr.strip()}
    return {"seconds": round(perf_counter() - start, 3), **value}


def normalize(record, home):
    for item in record.get("result", {}).get("evidence", []):
        item["path"] = Path(item["path"]).relative_to(home).as_posix()
    return record


def extract(cli, chat, eligible):
    start = perf_counter()
    passes = []
    for attempt in range(max_passes):
        remaining = max_extract_seconds - (perf_counter() - start)
        if remaining <= 0:
            break
        retry = ["--retry"] if attempt else []
        attempt_result = cli("extract-graph", "--model-endpoint", chat, "--limit", max_chunks, *retry, timeout=remaining)
        passes.append(attempt_result)
        result = attempt_result.get("result")
        if not result or result.get("provider_status") or not result.get("pending"):
            break
    elapsed = round(perf_counter() - start, 3)
    results = [report["result"] for report in passes if "result" in report]
    completed = sum(result["completed_chunks"] for result in results)
    attempts = sum(result["completed_chunks"] + result["stale_chunks"] + len(result["failures"]) for result in results)
    return {"eligible": eligible, "completed": completed, "attempts": attempts, "seconds": elapsed,
            "passes": passes, "within_budget": len(results) == len(passes) and elapsed <= max_extract_seconds
            and attempts <= max_passes * eligible}


def run(binary, public, cache, directory, number, questions, runtime, profile):
    corpus = directory / "corpus"
    corpus.mkdir(parents=True)
    for name, text in public.items():
        if name.startswith("corpus/"):
            (corpus / Path(name).name).write_bytes(text.encode("utf-8"))
    state = directory / "state"
    chat = directory / "chat.json"
    write(chat, endpoint)

    def cli(*args, **options):
        return command(binary, *args, "--state-dir", state, **options)

    cli("init", "--upgrade")["result"]
    ingest = cli("ingest", directory / "corpus")
    eligible = cli("status")["result"]["chunks"]
    if not 0 < eligible <= max_chunks or ingest["result"]["deferred_sources"] or ingest["result"]["failures"]:
        raise ValueError("Graph fixture ingestion did not complete within the frozen chunk limit.")
    inference = ["--model-cache", cache, "--ort-library", runtime, "--ort-sha256", profile["ort_sha256"]]
    embed = cli("embed", "--limit", max_chunks, *inference)
    if embed["result"]["completed_chunks"] != eligible:
        raise ValueError("The pinned BGE model did not embed all fixture chunks.")
    extraction = extract(cli, chat, eligible)
    result = {"schema_version": 1, "run": number, "fixture_manifest_sha256": MANIFEST_SHA256, "build_sha256": sha(binary),
              "endpoint": endpoint, "model_sha256": MODEL_SHA256, "server_sha256": profile["server_sha256"],
              "ort_sha256": profile["ort_sha256"],
              "ingest": ingest, "embed": embed, "extraction": extraction, "questions": []}
    common = ["--mode", "hybrid", "--limit", 5, *inference]
    modes = [("off", []), ("on", ["--graph"])]
    if number % 2 == 0:
        modes.reverse()
    for row in questions:
        kind = row.get("kind", "dev")
        item = {"id": row["id"], "kind": kind}
        for name, graph in modes:
            item[f"graph_{name}"] = normalize(cli("query", row["question"], *common, *graph, timeout=10), directory)
            if kind != "two_hop":
                item[f"answer_{name}"] = normalize(cli("answer", row["question"], "--model-endpoint", chat, *common, *graph, timeout=45), directory)
        result["questions"].append(item)
    question_id = "K03"
    filename = "corpus/crossref-14.md"
    before = next(row["graph_on"]["result"]["evidence"] for row in result["questions"]
                  if row["id"] == question_id)
    retired = [row for row in before if row["path"] == filename]
    if not retired:
        raise ValueError("The retirement control did not return the source before removal.")
    (directory / filename).unlink()
    sync = cli("ingest", corpus, "--sync")
    question = next(row["question"] for row in questions if row["id"] == question_id)
    after = normalize(cli("query", question, *common, "--graph", timeout=10), directory)
    result["retirement"] = {"id": question_id, "file": filename, "sync": sync, "after": after}
    result["retirement"]["passed"] = retirement_passed(result["retirement"], retired)
    if "result" in ingest:
        ingest["result"].pop("root", None)
    return result


def retirement_passed(retirement, retired):
    sync = retirement["sync"].get("result", {})
    after = retirement["after"].get("result")
    if (sync.get("removed_sources") != 1 or sync.get("sync_blocked") is not False
            or not isinstance(after, dict) or not isinstance(after.get("evidence"), list)):
        return False
    stale = {retirement["file"]}
    for row in retired:
        stale.update(row[field] for field in ("chunk_id", "revision", "source_id"))
    serialized = json.dumps(after, ensure_ascii=False)
    return not any(json.dumps(value, ensure_ascii=False) in serialized for value in stale)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--bge-files", type=Path, required=True)
    parser.add_argument("--ort-library", type=Path, required=True)
    parser.add_argument("--llama-server", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--profile", choices=PROFILES, required=True)
    parser.add_argument("--fixture", type=Path, default=DEFAULT_FIXTURE)
    parser.add_argument("--output", type=Path, required=True, help="A new local receipt directory.")
    args = parser.parse_args()
    try:
        profile = PROFILES[args.profile]
        if platform.system() != profile["system"] or platform.machine().lower() not in {"amd64", "x86_64"}:
            raise ValueError("The selected profile requires its named operating system and x86-64.")
        _, public = load_fixture(args.fixture)
        questions = json.loads(public["questions.json"])["questions"]
        binary = args.binary.resolve(strict=True)
        runtime = args.ort_library.resolve(strict=True)
        server = args.llama_server.resolve(strict=True)
        model = args.model.resolve(strict=True)
        if not args.bge_files.is_dir():
            raise ValueError("Supply a directory containing the six pinned BGE files.")
        for path, expected in (
            (runtime, profile["ort_sha256"]), (server, profile["server_sha256"]), (model, MODEL_SHA256),
        ):
            if sha(path) != expected:
                raise ValueError(f"SHA-256 mismatch for {path.name}.")
        args.output.mkdir()
        passed = True
        with tempfile.TemporaryDirectory(prefix="rust-graph-qualification-") as scratch:
            home = Path(scratch)
            cache = home / "bge"
            installed = command(binary, "model-install", "--cache", cache, "--from", args.bge_files)
            if "result" not in installed:
                raise ValueError("BGE import failed. Supply the six pinned files from model-manifest.")
            with local_server(server, model, profile):
                for number in range(1, 4):
                    result = run(binary, public, cache, home / f"run-{number}", number, questions, runtime, profile)
                    result["profile"] = args.profile
                    write(args.output / f"run-{number}.json", result)
                    extraction = result["extraction"]
                    retired = result["retirement"]["passed"]
                    passed = passed and retired
                    print(f"Run {number}: {extraction['completed']}/{extraction['eligible']} passages extracted, "
                          f"retirement {'passed' if retired else 'failed'}.", flush=True)
        if not passed:
            raise ValueError("A source retirement control failed. Review the receipts.")
    except (OSError, KeyError, ValueError) as error:
        parser.exit(1, f"Graph quality replay failed: {error}\n")


if __name__ == "__main__":
    main()
