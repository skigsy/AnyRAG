"""Verify a cited company Graph RAG answer against a synthetic deployment.

Uses invented notes and short-lived fixture identities with either a local
shared executable or a disposable loopback HTTPS service.
"""

import argparse
import http.client
import json
from pathlib import Path
import queue
import re
import shutil
import ssl
import subprocess
import tempfile
import time

from rehearse_shared import AUDIENCE, ISSUER, CheckFailed, Service, encoded, file_sha, identities, sha


QUESTION = "GA-9164"
ANSWER_QUESTION = (
    "Which catalog identifier does the equipment used by GA-9164 ultimately need? "
    "Give only that catalog identifier as the answer text, and cite both source passages."
)
TARGET = "KS-5815"
CANARY = "VioletOrchardCanary6759"
SEED = "mount-05.md"
FOLLOW = "crossref-07.md"
REFERENCE = {
    SEED: "# Current equipment assignment\n\nGA-9164 uses WQ-7945.\n\n"
          "Parts work must use the currently installed assembly reference.\n",
    FOLLOW: "# Reference catalog record\n\nWQ-7945 requires KS-5815.\n\n"
            "This entry specifies a ceramic bracket. The vendor label can differ from the catalog number.\n",
}
RESTRICTED = {
    "denied.md": f"# Restricted fixture\n\nGA-9164 requires a restricted item. {CANARY}.\n",
}
SUBJECTS = ("admin", "reader", "denied", "witness")
CITATION_FIELDS = ("chunk_id", "source_id", "revision", "source_hash", "start", "end",
                   "text", "path", "collection")


def write_json(path, value):
    path.write_bytes(encoded(value))


def write_fixture_identity(root, openssl):
    root.mkdir(mode=0o700)
    try:
        tokens = identities(openssl, root, (*SUBJECTS, "reader-control"))
        for subject, token in tokens.items():
            path = root / f"{subject}.jwt"
            with path.open("x", encoding="ascii") as output:
                path.chmod(0o600)
                output.write(token + "\n")
    except BaseException:
        shutil.rmtree(root)
        raise


def write_fixture_content(root, readers):
    for collection, notes in (("reference", REFERENCE), ("restricted", RESTRICTED)):
        folder = root / collection
        folder.mkdir()
        for name, text in notes.items():
            (folder / name).write_bytes(text.encode())
    policy_dir = root / "policy"
    policy_dir.mkdir(mode=0o700)
    versions = policy_dir / "versions"
    versions.mkdir()
    policy = {
        "schema_version": 1, "version": 1, "issuer": ISSUER, "administrators": ["admin"],
        "subjects": {}, "collections": {
            "reference": {"subjects": readers, "endpoints": ["relations", "answer"]},
            "restricted": {"subjects": ["witness"], "endpoints": []},
        },
    }
    write_json(versions / "1.json", policy)
    write_json(policy_dir / "current.json", policy)


def write_portable_fixture(root, openssl):
    root.mkdir(mode=0o700)
    try:
        write_fixture_content(root, ["reader", "reader-control"])
        write_fixture_identity(root / "identity", openssl)
    except BaseException:
        shutil.rmtree(root)
        raise


def prepare(root, args):
    write_fixture_content(root, ["reader"])
    for name in ("state", "backup"):
        (root / name).mkdir(mode=0o700)
    base = {
        "url": f"http://127.0.0.1:{args.model_port}/v1/chat/completions",
        "model": args.model_id, "response_format": "schema", "timeout_seconds": 120,
    }
    write_json(root / "relations.json", dict(base, max_tokens=512,
        predicates=["depends_on", "requires", "owned_by", "part_of", "uses"]))
    write_json(root / "answer.json", dict(base, max_tokens=384))
    profile = root / "profile.json"
    write_json(profile, {
        "schema_version": 1, "state_dir": str(root / "state"),
        "identity": {"issuer": ISSUER, "audience": AUDIENCE, "jwks_file": str(root / "jwks.json")},
        "policy": {"kind": "file", "directory": str(root / "policy")},
        "backup": {"kind": "file", "directory": str(root / "backup")},
        "sources": {name: {"kind": "folder", "root": str(root / name)}
                    for name in ("reference", "restricted")},
        "endpoints": {name: str(root / f"{name}.json") for name in ("relations", "answer")},
        "graph": {"mode": "typed", "extractor": "relations", "allow_unreviewed_model_links": True},
    })
    tokens = identities(args.openssl, root, SUBJECTS)
    return Service(args.binary, profile, tokens, request_timeout=135, sync_timeout=180)


def attach_fixture_service(args):
    tokens = {}
    for subject in SUBJECTS:
        with (args.fixture_identity / f"{subject}.jwt").open("rb") as stream:
            data = stream.read(8193)
        token = data.decode("ascii").strip()
        if len(data) > 8192 or re.fullmatch(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+){2}", token) is None:
            raise ValueError("Invalid fixture token")
        tokens[subject] = token
    context = ssl.create_default_context(cafile=str(args.fixture_ca))
    return Service(None, None, tokens, request_timeout=135, sync_timeout=180,
                   port=args.https_port, tls_context=context)


def healthy(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request("GET", "/health")
        response = connection.getresponse()
        if response.status != 200:
            return False
        body = json.loads(response.read(8192))
        return isinstance(body, dict) and body.get("status") == "ok"
    except (OSError, ValueError, http.client.HTTPException):
        return False
    finally:
        connection.close()


def exact(row, collection, notes):
    text = notes.get(row.get("path"), "")
    start, end = row.get("start"), row.get("end")
    return (row.get("collection") == collection and bool(text)
            and type(start) is int and type(end) is int and 0 <= start < end <= len(text)
            and row.get("text") == text[start:end]
            and row.get("source_hash") == row.get("revision") == sha(text.encode())
            and all(re.fullmatch(r"[0-9a-f]{64}", row.get(key, ""))
                    for key in ("chunk_id", "source_id")))


def same_citation(left, right):
    return all(left.get(key) == right.get(key) for key in CITATION_FIELDS)


def rehearse(service, receipt, started):
    def check(name, passed):
        receipt["checks"][name] = "pass" if passed else "fail"
        if not passed:
            raise CheckFailed(name)

    def query(graph, role="reader", *, answer=False):
        payload = {"question": ANSWER_QUESTION if answer else QUESTION,
                   "mode": "lexical", "graph": graph, "limit": 5}
        if answer:
            payload["answer_endpoint"] = "answer"
        return service.call("/v1/query", payload, role)

    check("anonymous_query_refused", query(False, None)[0] == 401)
    check("reader_sync_refused", service.call("/v1/admin/sync", {}, "reader")[0] == 403)
    sync = service.sync()
    check("sync_and_typed_extraction", sync["status"] == "complete"
          and sync["updated"] == 3 and sync["graph_pending"] == 0
          and set(sync["enrichment"]) == {"reference"})

    off_code, off = query(False)
    on_code, on = query(True)
    rows = {row["path"]: row for row in on["evidence"]}
    check("graph_finds_second_current_passage", off_code == on_code == 200
          and [row["path"] for row in off["evidence"]] == [SEED]
          and len(on["evidence"]) == 2 and rows.keys() == REFERENCE.keys()
          and all(exact(row, "reference", REFERENCE) for row in rows.values()))
    seed, follow = rows[SEED], rows[FOLLOW]
    ids = [seed["chunk_id"], follow["chunk_id"]]
    support = follow["graph_support"]
    assertion = support["assertion"]
    check("same_collection_typed_chain", support["hop"] == 1 and support["seed_chunk_id"] == ids[0]
          and support["supporting_chunk_ids"] == ids
          and (assertion["subject"], assertion["predicate"], assertion["object"])
          == ("WQ-7945", "requires", TARGET)
          and REFERENCE[FOLLOW][assertion["start"]:assertion["end"]]
          == assertion["quote"] == "WQ-7945 requires KS-5815.")
    for row in (seed, follow):
        status, reply = service.cite(row)
        cite = reply.get("evidence", {})
        check(f"direct_{row['path']}_citation", status == 200
              and exact(cite, "reference", REFERENCE) and same_citation(cite, row))
    receipt["first_verified_citation_seconds"] = round(time.monotonic() - started, 3)

    status, reply = query(True, answer=True)
    answer = reply.get("answer", {})
    answer_rows = reply.get("evidence", [])
    answer_citations = answer.get("citations", [])
    answer_text = (answer.get("answer") or "").strip().removesuffix(".")
    check("generated_answer_with_both_verified_citations", status == 200
          and reply.get("answer_generated") is True and answer.get("status") == "answered"
          and answer_text == TARGET
          and len(answer_citations) == len(ids) and set(answer_citations) == set(ids)
          and len(answer_rows) == len(ids) and {row["chunk_id"] for row in answer_rows} == set(ids)
          and all(same_citation(row, rows.get(row.get("path"), {})) for row in answer_rows)
          and CANARY not in json.dumps(reply))
    receipt["answer"] = {"catalog_id": TARGET, "verified_citations": len(ids)}
    status, witness = query(False, "witness")
    check("restricted_control_is_indexed", status == 200 and len(witness["evidence"]) == 1
          and exact(witness["evidence"][0], "restricted", RESTRICTED))
    denied_status, denied = query(True, "denied")
    check("restricted_citation_and_search_denied", denied_status == 200 and denied["evidence"] == []
          and service.cite(witness["evidence"][0])[0] == 404
          and service.cite(seed, "denied")[0] == 404)
    status, denial = query(True, "witness", answer=True)
    check("restricted_answer_destination_denied", status == 403
          and denial.get("error", {}).get("code") == "MODEL_DESTINATION_DENIED")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--binary", type=Path)
    mode.add_argument("--https-port", type=int, help="port of a fresh disposable HTTPS service on 127.0.0.1")
    mode.add_argument("--write-fixture-identity", type=Path,
                      help="create public JWKS and ten-minute synthetic tokens in a new private directory")
    mode.add_argument("--write-portable-fixture", type=Path,
                      help="create synthetic notes, policy and identity in a new private directory")
    parser.add_argument("--receipt", type=Path, help="new file for the redacted result")
    parser.add_argument("--fixture-identity", type=Path, help="fixture token directory for HTTPS mode")
    parser.add_argument("--fixture-ca", type=Path, help="trusted fixture certificate or CA for HTTPS mode")
    parser.add_argument("--model-port", type=int, default=8081, help="local llama.cpp HTTP port")
    parser.add_argument("--model-id", default="local-qwen", help="model alias configured in llama.cpp")
    parser.add_argument("--openssl", default="openssl")
    args = parser.parse_args()
    tls = args.https_port is not None
    if tls:
        if not 1 <= args.https_port <= 65535 or args.fixture_identity is None or args.fixture_ca is None:
            parser.error("HTTPS mode requires a valid port, --fixture-identity and --fixture-ca")
    elif args.fixture_identity is not None or args.fixture_ca is not None:
        parser.error("fixture identity and CA inputs require --https-port")
    if args.write_fixture_identity is not None or args.write_portable_fixture is not None:
        if args.receipt is not None:
            parser.error("fixture creation does not use --receipt")
        try:
            if args.write_portable_fixture is None:
                write_fixture_identity(args.write_fixture_identity, args.openssl)
                print("Created public JWKS and short-lived synthetic tokens.")
            else:
                write_portable_fixture(args.write_portable_fixture, args.openssl)
                print("Created synthetic notes, policy, public JWKS and short-lived tokens.")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            parser.exit(1, f"Could not create the fixture ({type(error).__name__}).\n")
        return 0
    if args.receipt is None:
        parser.error("provide --receipt for the company rehearsal")
    if not tls and (not args.binary.is_file() or not 1 <= args.model_port <= 65535):
        parser.error("provide an existing shared binary and a valid local model port")
    receipt = {"schema_version": 1, "status": "incomplete", "fixture": "company-two-edge-v1",
               "runner_sha256": file_sha(Path(__file__)),
               "shared_helper_sha256": file_sha(Path(__file__).with_name("rehearse_shared.py")),
               "notes_sha256": {name: sha(text.encode()) for name, text in
                                (REFERENCE | RESTRICTED).items()},
               "checks": {}}
    if tls:
        receipt["scope"] = "synthetic signed loopback HTTPS service with fixture identities"
    else:
        args.binary = args.binary.resolve()
        receipt.update(binary_sha256=file_sha(args.binary), model_alias_sha256=sha(args.model_id.encode()),
                       scope="synthetic signed loopback service with a separately launched local model")
    started = time.monotonic()
    with args.receipt.open("x", encoding="utf-8") as output:
        try:
            if tls:
                receipt["fixture_ca_sha256"] = file_sha(args.fixture_ca)
                rehearse(attach_fixture_service(args), receipt, started)
            else:
                model_ready = healthy(args.model_port)
                receipt["checks"]["local_model_health"] = "pass" if model_ready else "fail"
                if not model_ready:
                    raise CheckFailed("local_model_health")
                with tempfile.TemporaryDirectory(prefix="rust-rag-company-") as temp:
                    service = prepare(Path(temp), args)
                    try:
                        service.start()
                        rehearse(service, receipt, started)
                    finally:
                        service.stop()
            receipt["status"] = "pass"
        except CheckFailed as error:
            receipt.update(status="fail", failed_check=str(error))
        except KeyboardInterrupt:
            receipt["driver_error"] = "Interrupted"
        except (OSError, ValueError, KeyError, TypeError, RuntimeError,
                http.client.HTTPException, subprocess.SubprocessError, queue.Empty) as error:
            receipt["driver_error"] = type(error).__name__
        receipt["elapsed_seconds"] = round(time.monotonic() - started, 3)
        output.write(json.dumps(receipt, indent=2) + "\n")
    print(f"Company Graph RAG rehearsal {receipt['status']}.")
    if receipt["status"] != "pass":
        print(receipt.get("failed_check", receipt.get("driver_error")))
    return 0 if receipt["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
