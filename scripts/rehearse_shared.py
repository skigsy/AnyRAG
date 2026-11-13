"""Rehearse the shared Graph RAG CLI against invented data and a loopback model.

Requires Python 3.10+, OpenSSL, and a rust-rag binary built with the shared feature.
The oracle evaluates public HTTP responses; fixture model captures stay in memory.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass, replace
import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import secrets
import subprocess
import tempfile
import threading
import time


ISSUER = "https://issuer.example.invalid"
AUDIENCE = "rust-rag-rehearsal"
MODEL = "cedar-fixture-v1"
QUESTION = "CedarPump"
REQUIRED = (
    "initial_sync", "keyword_baseline", "two_hop_anchors", "denied_surfaces",
    "scoped_model_input", "backup_saved", "revoke_before_dispatch",
    "revoke_after_dispatch", "current_grants", "stale_extraction_withheld", "edited_anchor",
    "edited_citation_retired", "dependent_edge_retired", "index_only_removal",
    "index_only_reingest", "authoritative_delete", "restore_closed",
    "unreadable_reconcile", "interrupted_recovery", "current_inventory",
    "current_policy_after_reopen", "remaining_graph_path", "retained_backup",
)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def file_sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@dataclass(frozen=True)
class Note:
    path: str
    subject: str
    target: str
    marker: str = ""

    @property
    def quote(self):
        return f"{self.subject} requires {self.target}."

    @property
    def prefix(self):
        return "Préface. " + (f"{self.marker}. " if self.marker else "")

    @property
    def text(self):
        return self.prefix + self.quote

    @property
    def revision(self):
        return sha(self.text.encode())

    def write(self, folder):
        (folder / self.path).write_bytes(self.text.encode())


PUMP = Note("pump.md", "CedarPump", "RelayKit")
RELAY = Note("relay.md", "RelayKit", "SpareBracket")
BRACKET = Note("bracket.md", "SpareBracket", "InspectionTag")
STALE = Note("relay.md", "RelayKit", "StaleLatch")
EDITED = Note("relay.md", "RelayKit", "WeatherGasket")
INTERRUPTED = Note("relay.md", "RelayKit", "RecoveryLatch")
CURRENT = Note("relay.md", "RelayKit", "ServiceLatch")
CURRENT_BRIDGE = Note("bracket.md", "ServiceLatch", "ServiceTag")


def exact_anchor(row, note, collection):
    return (
        row.get("path") == note.path
        and row.get("collection") == collection
        and row.get("text") == note.text
        and row.get("start") == 0
        and row.get("end") == len(note.text)
        and row.get("source_hash") == row.get("revision") == note.revision
        and row.get("page") is None
        and row.get("anchor") is None
        and all(re.fullmatch(r"[0-9a-f]{64}", row.get(key, "")) for key in ("source_id", "chunk_id"))
    )


def exact_hop(row, note, hop, path):
    support = row.get("graph_support", {})
    assertion = support.get("assertion", {})
    start = len(note.prefix)
    end = len(note.text)
    return (
        support.get("hop") == hop
        and support.get("seed_chunk_id") == path[0]
        and support.get("supporting_chunk_ids") == path
        and assertion.get("subject") == note.subject
        and assertion.get("predicate") == "requires"
        and assertion.get("object") == note.target
        and assertion.get("quote") == note.text[start:end]
        and (assertion.get("start"), assertion.get("end")) == (start, end)
    )


class CheckFailed(Exception):
    pass


class Oracle:
    def __init__(self):
        self.observations = {}
        self.reducers = {}
        self.faults = []

    @classmethod
    def matches(cls, expected, actual):
        if type(expected) is not type(actual):
            return False
        if isinstance(expected, dict):
            return expected.keys() == actual.keys() and all(
                cls.matches(value, actual[key]) for key, value in expected.items()
            )
        return expected == actual

    @classmethod
    def public_actual(cls, expected, actual):
        if isinstance(expected, dict) and isinstance(actual, dict):
            return {key: cls.public_actual(value, actual.get(key)) for key, value in expected.items()}
        if actual is None or type(actual) in (int, bool) or cls.matches(expected, actual):
            return actual
        # A broken implementation could put arbitrary text in a status field.
        return "unexpected"

    def check(self, name, observation, expected=True, *, reduce=None):
        if reduce is not None:
            self.reducers[name] = reduce
        actual = observation if reduce is None else reduce(observation)
        passed = self.matches(expected, actual)
        self.observations[name] = {"expected": expected, "actual": self.public_actual(expected, actual),
                                   "status": "pass" if passed else "fail"}
        if not passed:
            raise CheckFailed(name)

    def inject(self, fault, check, observed):
        # Replace the observed value at the oracle boundary using a valid
        # result captured at an earlier stage. The binary is untouched.
        expected = self.observations[check]["expected"]
        caught = not self.matches(expected, self.reducers[check](observed))
        self.faults.append({"fault": fault, "check": check, "method": "observation_replay", "caught": caught})
        if not caught:
            raise CheckFailed(fault)

    def results(self):
        return [
            {"check": name, **self.observations.get(name, {"status": "unexercised"})}
            for name in REQUIRED
        ]


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=")


def identities(openssl, root, subjects=("admin", "reader")):
    key = root / "fixture-signing-key.pem"

    def command(*args, input=None):
        return subprocess.run([openssl, *map(str, args)], input=input, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, check=True, timeout=15).stdout

    command("genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048",
            "-pkeyopt", "rsa_keygen_pubexp:65537", "-out", key)
    try:
        modulus = command("rsa", "-in", key, "-noout", "-modulus").strip()
        if not modulus.startswith(b"Modulus="):
            raise ValueError("Unexpected synthetic key format")
        public = {"kty": "RSA", "kid": "rehearsal", "alg": "RS256", "use": "sig",
                  "n": b64(bytes.fromhex(modulus.split(b"=", 1)[1].decode())).decode(), "e": "AQAB"}
        (root / "jwks.json").write_bytes(encoded({"keys": [public]}))
        tokens = {}
        for subject in subjects:
            header = {"alg": "RS256", "kid": "rehearsal", "typ": "at+jwt"}
            claims = {"iss": ISSUER, "aud": AUDIENCE, "sub": subject, "exp": int(time.time()) + 600}
            payload = b".".join(map(b64, (encoded(header), encoded(claims))))
            signature = command("dgst", "-sha256", "-sign", key, input=payload)
            tokens[subject] = b".".join((payload, b64(signature))).decode()
        return tokens
    finally:
        key.unlink()


class Barrier:
    def __init__(self, predicate):
        self.predicate = predicate
        self.entered = threading.Event()
        self.release = threading.Event()

    def wait(self):
        if not self.entered.wait(6):
            raise TimeoutError("Expected model request did not arrive")


class Model:
    def __init__(self, notes):
        self.captures = []
        self.barrier = None
        self.lock = threading.Lock()
        self.notes = {note.text: note for note in notes}

    def hold(self, predicate):
        self.barrier = Barrier(predicate)
        return self.barrier

    def dispatch(self, path, body):
        if path == "/v1/embeddings":
            context = {"input": body["input"]}
            reply = {"data": [{"index": 0, "embedding": [1.0, 0.0]}]}
        else:
            if path != "/v1/chat/completions":
                raise ValueError("Unexpected model endpoint")
            context = json.loads(body["messages"][1]["content"])
            if "passage" in context:
                note = self.notes.get(context["passage"])
                result = {"assertions": [] if note is None else [
                    {"subject": note.subject, "predicate": "requires", "object": note.target, "quote": note.quote}
                ]}
            else:
                result = {"status": "answered", "answer": "Synthetic cited result.",
                          "citations": [row["id"] for row in context["evidence"]]}
            reply = {"choices": [{"finish_reason": "stop", "message": {"content": json.dumps(result)}}]}
        with self.lock:
            self.captures.append(context)
            barrier = self.barrier
            if barrier is not None and barrier.predicate(context):
                self.barrier = None
                barrier.entered.set()
            else:
                barrier = None
        if barrier is not None and not barrier.release.wait(8):
            raise TimeoutError("Model barrier timed out")
        return reply

    def serve(self):
        model = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    size = int(self.headers["Content-Length"])
                    if not 0 < size <= 128 * 1024:
                        self.send_error(413)
                        return
                    result = encoded(model.dispatch(self.path, json.loads(self.rfile.read(size))))
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(result)))
                    self.end_headers()
                    self.wfile.write(result)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Expected when the rehearsal stops an in-flight service.
                except (KeyError, TypeError, ValueError, TimeoutError):
                    self.send_error(503)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        return server


class Service:
    def __init__(self, binary, profile, tokens, *, request_timeout=10, sync_timeout=15,
                 port=None, tls_context=None):
        self.binary = binary
        self.profile = profile
        self.tokens = tokens
        self.request_timeout = request_timeout
        self.sync_timeout = sync_timeout
        self.port = port
        self.tls_context = tls_context
        self.process = None

    def start(self):
        lines = queue.Queue()
        process = self.process = subprocess.Popen(
            [self.binary, "serve", "--profile", self.profile, "--bind", "127.0.0.1:0"],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )

        def observe_start():
            for line in process.stderr:
                if match := re.fullmatch(r"Rust RAG shared service listening on 127\.0\.0\.1:(\d+)\s*", line):
                    lines.put(int(match[1]))
            lines.put(None)

        threading.Thread(target=observe_start, daemon=True).start()
        self.port = lines.get(timeout=12)
        if self.port is None:
            raise RuntimeError("Shared binary did not bind")

    def stop(self):
        if self.process:
            self.process.kill()
            self.process.wait(timeout=8)
            self.process.stderr.close()
            self.process = None

    def call(self, path, body=None, role="reader"):
        if self.tls_context is None:
            connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=self.request_timeout)
        else:
            connection = http.client.HTTPSConnection("127.0.0.1", self.port,
                timeout=self.request_timeout, context=self.tls_context)
        headers = {"Content-Type": "application/json"}
        if role is not None:
            headers["Authorization"] = f"Bearer {self.tokens[role]}"
        try:
            connection.request("GET" if body is None else "POST", path,
                               body=None if body is None else encoded(body), headers=headers)
            response = connection.getresponse()
            return response.status, json.loads(response.read(1024 * 1024))
        finally:
            connection.close()

    def admin(self, action, body=None):
        return self.call(f"/v1/admin/{action}", body, role="admin")

    def query(self, role="reader", *, graph=True, mode="lexical", answer=False):
        payload = {"question": QUESTION, "mode": mode, "graph": graph, "limit": 5}
        if answer:
            payload["answer_endpoint"] = "answer"
        return self.call("/v1/query", payload, role)

    def cite(self, row, role="reader"):
        return self.call(f"/v1/evidence/{row['chunk_id']}", role=role)

    def sync(self, wait=True):
        if self.admin("sync", {})[0] != 202:
            raise RuntimeError("Sync was not admitted")
        if wait:
            return self.await_sync()

    def await_sync(self):
        end = time.monotonic() + self.sync_timeout
        while time.monotonic() < end:
            code, status = self.admin("status")
            if code != 200:
                raise RuntimeError("Cannot read sync status")
            job = status["sync"]
            if job["status"] != "running":
                return job
            time.sleep(0.025)
        raise TimeoutError("Sync did not finish")


class Fixture:
    def __init__(self, root, openssl, model_port):
        self.root = root
        self.open = root / "open"
        self.private = root / "private"
        self.policy = root / "policy"
        self.backup = root / "backup"
        self.profile = root / "profile.json"
        private_mode = 0o700 if os.name == "posix" else 0o777
        self.policy.mkdir(mode=private_mode)
        self.backup.mkdir(mode=private_mode)
        for folder in (self.open, self.private, self.policy / "versions"):
            folder.mkdir()
        self.tokens = identities(openssl, root)
        self.policies = []
        self.set_grants(["reader"], ["admin"])
        base = f"http://127.0.0.1:{model_port}/v1"
        for name in ("extract", "answer", "embedding"):
            config = {"url": f"{base}/chat/completions", "model": MODEL, "timeout_seconds": 9}
            if name == "extract":
                config["predicates"] = ["requires"]
            if name == "embedding":
                config.update(url=f"{base}/embeddings", dimensions=2)
            (root / f"{name}.json").write_bytes(encoded(config))
        profile = {
            "schema_version": 1, "state_dir": str(root / "state"),
            "identity": {"issuer": ISSUER, "audience": AUDIENCE, "jwks_file": str(root / "jwks.json")},
            "policy": {"kind": "file", "directory": str(self.policy)},
            "backup": {"kind": "file", "directory": str(self.backup)},
            "sources": {name: {"kind": "folder", "root": str(root / name)} for name in ("open", "private")},
            "endpoints": {name: str(root / f"{name}.json") for name in ("extract", "answer")},
            "embedding": {"provider": "endpoint", "name": "embedding", "config": str(root / "embedding.json")},
            "graph": {"mode": "typed", "extractor": "extract"},
        }
        self.profile.write_bytes(encoded(profile))

    def set_grants(self, open_subjects, private_subjects, *, extract=True):
        version = len(self.policies) + 1
        value = {"schema_version": 1, "version": version, "issuer": ISSUER,
                 "administrators": ["admin"], "subjects": {}, "collections": {
                     "open": {"subjects": open_subjects,
                              "endpoints": (["extract"] if extract else []) + ["answer", "embedding"]},
                     "private": {"subjects": private_subjects, "endpoints": ["extract"]},
                 }}
        data = encoded(value)
        (self.policy / "versions" / f"{version}.json").write_bytes(data)
        next_policy = self.policy / "next.json"
        next_policy.write_bytes(data)
        os.replace(next_policy, self.policy / "current.json")
        self.policies.append(sha(data))


def by_path(result):
    return {row["path"]: row for row in result.get("evidence", [])}


def graph_chain(service, result, notes, role="reader"):
    indexed = by_path(result)
    if set(indexed) != {note.path for note in notes} or len(result["evidence"]) != len(notes):
        return False
    path = []
    for index, note in enumerate(notes):
        row = indexed[note.path]
        if not exact_anchor(row, note, "open"):
            return False
        code, response = service.cite(row, role)
        citation = response.get("evidence", {})
        if code != 200 or not exact_anchor(citation, note, "open") or citation.get("chunk_id") != row["chunk_id"]:
            return False
        path.append(row["chunk_id"])
        if index and not exact_hop(row, note, index, path):
            return False
    coverage = result.get("graph_coverage", {}).get("open", {})
    return coverage.get("seed_status") == "seeded" and coverage.get("partial") is False


def lifecycle(service, fixture, model, retired, denied, oracle):
    check = oracle.check
    for note in (PUMP, retired, BRACKET):
        note.write(fixture.open)
    denied.write(fixture.private)
    service.start()
    initial = service.sync()
    check("initial_sync", {key: initial.get(key) for key in ("status", "updated", "graph_pending", "vector_pending")},
          {"status": "complete", "updated": 4, "graph_pending": 0, "vector_pending": 0})
    code, keyword = service.query(graph=False)
    check("keyword_baseline", code == 200 and set(by_path(keyword)) == {PUMP.path}
          and exact_anchor(by_path(keyword)[PUMP.path], PUMP, "open"))
    code, baseline = service.query()
    check("two_hop_anchors", code == 200 and graph_chain(service, baseline, (PUMP, retired, BRACKET)))
    old = by_path(baseline)[retired.path]
    old_bracket = by_path(baseline)[BRACKET.path]
    old_cite = service.cite(old)
    code, private = service.query("admin")
    restricted = by_path(private).get(denied.path, {})
    check("denied_surfaces", code == 200 and exact_anchor(restricted, denied, "private")
          and private.get("graph_coverage", {}).get("private", {}).get("seed_status") == "seeded"
          and service.cite(restricted, "admin")[0] == 200 and service.cite(restricted)[0] == 404
          and denied.target not in json.dumps((baseline, keyword)))
    mark = len(model.captures)
    answered_response = service.query(answer=True)
    code, answered = answered_response
    captured = model.captures[mark:]
    expected_ids = {row["chunk_id"] for row in baseline["evidence"]}
    check("scoped_model_input", code == 200 and answered.get("answer", {}).get("status") == "answered"
          and set(answered["answer"]["citations"]) == expected_ids and len(captured) == 1
          and {row["passage"] for row in captured[0].get("evidence", [])} == {PUMP.text, retired.text, BRACKET.text}
          and denied.target not in json.dumps(captured))
    code, saved = service.admin("backup", {})
    backup_id = saved.get("backup_id", "")
    check("backup_saved", code == 200 and saved.get("status") == "saved"
          and bool(re.fullmatch(r"[0-9a-f]{32}", backup_id)))

    def revocation(observation):
        (code, body), requests = observation
        return {"status": code, "error": body.get("error", {}).get("code"),
                "question_only": requests == [{"input": QUESTION}],
                "answer_dispatched": len(requests) == 1 and "evidence" in requests[0],
                "withheld": "evidence" not in body and "answer" not in body,
                "denied_payload": denied.target in json.dumps(requests)}

    with ThreadPoolExecutor(max_workers=1) as caller:
        for before_dispatch in (True, False):
            mark = len(model.captures)
            barrier = model.hold(lambda payload: ("input" if before_dispatch else "evidence") in payload)
            pending = caller.submit(service.query, mode="hybrid" if before_dispatch else "lexical", answer=True)
            try:
                barrier.wait()
                fixture.set_grants([], ["admin"])
            finally:
                barrier.release.set()
            response = pending.result(timeout=10)
            captured = model.captures[mark:]
            key = "revoke_before_dispatch" if before_dispatch else "revoke_after_dispatch"
            expected = {"status": 409, "error": "ACCESS_CHANGED", "question_only": before_dispatch,
                        "answer_dispatched": not before_dispatch, "withheld": True, "denied_payload": False}
            check(key, (response, captured), expected, reduce=revocation)
            if not before_dispatch:
                after_dispatch = captured
            mark = len(model.captures)
            code, revoked = service.query(answer=True)
            if code != 200 or revoked.get("evidence") != [] or service.cite(old)[0] != 404 or len(model.captures) != mark:
                check("current_grants", False)
            fixture.set_grants(["reader"], ["admin"])
    check("current_grants", service.cite(old)[0] == 200)

    # The extractor has already received this revision. Removing approval
    # while its response is held must prevent assertions from publishing.
    STALE.write(fixture.open)
    barrier = model.hold(lambda payload: payload.get("passage") == STALE.text)
    try:
        service.sync(wait=False)
        barrier.wait()
        fixture.set_grants(["reader"], ["admin"], extract=False)
    finally:
        barrier.release.set()
    stale = service.await_sync()
    code, pending_graph = service.query()
    check("stale_extraction_withheld", stale.get("status") == "incomplete"
          and stale.get("reason") == "ACCESS_CHANGED" and code == 200
          and set(by_path(pending_graph)) == {PUMP.path}
          and pending_graph.get("graph_coverage", {}).get("open", {}).get("index_pending") is True)
    fixture.set_grants(["reader"], ["admin"])

    EDITED.write(fixture.open)
    edited_sync = service.sync()
    code, edited = service.query()
    edited_row = by_path(edited).get(EDITED.path, {})
    check("edited_anchor", code == 200 and edited_sync.get("status") == "complete"
          and graph_chain(service, edited, (PUMP, EDITED)) and edited_row.get("revision") != old["revision"])
    check("edited_citation_retired", service.cite(old), 404, reduce=lambda response: response[0])

    def retired_edges(result):
        visible = json.dumps(result)
        return {"old_revision_visible": old["revision"] in visible,
                "old_bridge_visible": old_bracket["chunk_id"] in visible,
                "retired_canary_visible": retired.marker in visible}

    check("dependent_edge_retired", edited,
          {"old_revision_visible": False, "old_bridge_visible": False, "retired_canary_visible": False},
          reduce=retired_edges)
    code, removal = service.admin("remove", {"source_id": edited_row["source_id"]})
    absent_code, absent = service.query()
    check("index_only_removal", code == 200 and removal.get("removed_sources") == 1
          and absent_code == 200 and set(by_path(absent)) == {PUMP.path}
          and service.cite(edited_row)[0] == 404 and (fixture.open / EDITED.path).is_file())
    reingested = service.sync()
    code, found = service.query()
    check("index_only_reingest", reingested.get("status") == "complete" and code == 200
          and graph_chain(service, found, (PUMP, EDITED)))
    (fixture.open / EDITED.path).unlink()
    deleted = service.sync()
    repeated = service.sync()
    code, gone = service.query()
    check("authoritative_delete", deleted.get("removed") == 1 and repeated.get("status") == "complete"
          and code == 200 and set(by_path(gone)) == {PUMP.path} and service.cite(edited_row)[0] == 404)

    # Change access since the snapshot. Current policy and source inventory
    # remain independent of the backup's indexed collections.
    fixture.set_grants(["admin"], [])
    def recovery(observation):
        restored, query, citation, reopen, after_reopen = observation
        return {"restored": restored[0], "maintenance": restored[1].get("maintenance"),
                "query": query[0], "citation": citation[0], "reopen": reopen[0],
                "query_after_reopen": after_reopen[0]}

    restored = service.admin("restore", {"backup_id": backup_id})
    closed = (restored, service.query("admin"), service.cite(old, "admin"),
              service.admin("reopen", {}), service.query("admin"))
    check("restore_closed", closed,
          {"restored": 200, "maintenance": True, "query": 503, "citation": 503, "reopen": 409,
           "query_after_reopen": 503}, reduce=recovery)
    unreadable = fixture.open / "invalid.md"
    unreadable.write_bytes(b"\xff\xfe\xff")
    incomplete = service.sync()
    check("unreadable_reconcile", incomplete.get("status") == "partial"
          and incomplete.get("failed", 0) >= 1 and incomplete.get("inventory_reconciled") is False
          and service.admin("reopen", {})[0] == 409 and service.query("admin")[0] == 503)
    unreadable.unlink()
    INTERRUPTED.write(fixture.open)
    barrier = model.hold(lambda payload: payload.get("passage") == INTERRUPTED.text)
    try:
        service.sync(wait=False)
        barrier.wait()
        service.stop()
    finally:
        barrier.release.set()
    service.start()
    code, state = service.admin("status")
    check("interrupted_recovery", code == 200 and state.get("maintenance") is True
          and service.query("admin")[0] == 503 and service.admin("reopen", {})[0] == 409)
    # Retire the abandoned claim by replacing that exact revision. The
    # fixture does not wait for the same-revision lease to expire.
    CURRENT.write(fixture.open)
    CURRENT_BRIDGE.write(fixture.open)
    (fixture.private / denied.path).unlink()
    reconciled = service.sync()
    check("current_inventory", reconciled.get("inventory_reconciled") is True
          and reconciled.get("status") == "complete" and reconciled.get("graph_pending") == 0
          and service.query("admin")[0] == 503)
    reopened = service.admin("reopen", {})
    current_response = service.query("admin")
    code, current = current_response
    reader_code, reader = service.query()
    check("current_policy_after_reopen", reopened[0] == 200 and code == 200 and reader_code == 200
          and reader.get("evidence") == [] and service.cite(by_path(current)[PUMP.path])[0] == 404
          and all(service.cite(row, "admin")[0] == 404 for row in (old, old_bracket, edited_row, restricted))
          and all(marker not in json.dumps(current) for marker in (denied.target, retired.marker)))
    check("remaining_graph_path", graph_chain(service, current, (PUMP, CURRENT, CURRENT_BRIDGE), "admin"))
    check("retained_backup", retired.marker.encode() in (fixture.backup / backup_id / "open.sqlite3").read_bytes()
          and denied.target.encode() in (fixture.backup / backup_id / "private.sqlite3").read_bytes())

    oracle.inject("disabled_revision_checks", "edited_citation_retired", old_cite)
    oracle.inject("disabled_graph_retirement", "dependent_edge_retired", baseline)
    oracle.inject("disabled_revocation_rechecks", "revoke_after_dispatch", (answered_response, after_dispatch))
    oracle.inject("premature_restore_reopen", "restore_closed", (*closed[:3], reopened, current_response))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--openssl", default="openssl", help="OpenSSL executable for short-lived fixture identity")
    parser.add_argument("--receipt", type=Path, required=True, help="new file for the redacted JSON receipt")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    oracle = Oracle()
    started = time.monotonic()
    denied = Note("private.md", "CedarPump", f"Private{secrets.token_hex(8)}")
    retired = replace(RELAY, marker=f"Retired{secrets.token_hex(8)}")
    other_notes = (PUMP, BRACKET, STALE, EDITED, INTERRUPTED, CURRENT, CURRENT_BRIDGE)
    notes = (retired, denied, *other_notes)
    model = Model(notes)
    receipt = {
        "schema_version": 1, "profile": "local-shared-folder-typed-v1", "binary_sha256": file_sha(binary),
        "fixture": {"id": "cedarpump-shared-v1", "sha256": sha(encoded([asdict(note) for note in notes])),
                    "spec_sha256": sha(encoded([asdict(note) for note in (RELAY, *other_notes)]))},
        "model": {"id": MODEL, "kind": "controlled_loopback", "implementation_sha256": sha(Path(__file__).read_bytes())},
        "coverage_limits": ["pinned real-model quality", "live identity and cloud",
                            "incomplete directory enumeration", "same-revision lease expiry",
                            "mid-copy snapshot interruption",
                            "erasure from backups or provider records"],
    }
    with args.receipt.open("x", encoding="utf-8") as output, tempfile.TemporaryDirectory(prefix="rust-rag-rehearsal-") as temp:
        server = model.serve()
        service = None
        try:
            fixture = Fixture(Path(temp), args.openssl, server.server_port)
            receipt["policy_sha256"] = fixture.policies
            receipt["profile_sha256"] = file_sha(fixture.profile)
            service = Service(binary, fixture.profile, fixture.tokens)
            lifecycle(service, fixture, model, retired, denied, oracle)
            receipt["status"] = "pass"
        except CheckFailed as error:
            receipt.update(status="fail", failed_check=str(error))
        except KeyboardInterrupt:
            receipt.update(status="incomplete", driver_error="Interrupted")
        except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError, queue.Empty) as error:
            receipt.update(status="incomplete", driver_error=type(error).__name__)
        finally:
            if service is not None:
                service.stop()
            server.shutdown()
            server.server_close()
        results = oracle.results()
        receipt.update(checks=results, fault_injections=oracle.faults, elapsed_seconds=round(time.monotonic() - started, 3))
        output.write(json.dumps(receipt, indent=2) + "\n")
    passed = sum(item["status"] == "pass" for item in results)
    caught = sum(item["caught"] for item in oracle.faults)
    print(f"Shared Graph RAG rehearsal {receipt['status']}. {passed}/{len(REQUIRED)} checks passed, {caught}/4 injected faults caught.")
    if receipt["status"] != "pass":
        print(receipt.get("failed_check", receipt.get("driver_error")))
    return 0 if receipt["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
