"""Reproduce personal citations and graph review with a disposable loopback fixture."""

import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import time

from rehearse_shared import (
    BRACKET, MODEL, PUMP, QUESTION, RELAY, CheckFailed, Fixture, Model, Note,
    Service, by_path, encoded, file_sha,
)


PROSE = Note("prose.md", "RelayKit", "WeatherGasket")
HIGH = "RelayKit | owned_by | ServiceDesk"
UPDATED = "RelayKit | requires | ReplacementBracket"


class OperatorModel(Model):
    def dispatch(self, path, body):
        if path == "/v1/chat/completions":
            context = json.loads(body["messages"][1]["content"])
            if "checks" in context:
                expected = {note.quote for note in (PUMP, RELAY, BRACKET)
                            if context["document"] == note.text}
                decisions = [
                    {"id": row["id"], "entities": "yes",
                     "affirmative": "yes" if row["quote"] in expected else "no"}
                    for row in context["checks"]
                ]
                return {"choices": [{"finish_reason": "stop", "message": {
                    "content": json.dumps({"decisions": decisions}),
                }}]}
        return super().dispatch(path, body)


def cli(binary, state, *args, refused=False):
    result = subprocess.run(
        [binary, "--state-dir", state, *map(str, args), "--json"],
        stdin=subprocess.DEVNULL, capture_output=True, encoding="utf-8", timeout=25,
    )
    if (result.returncode != 0) != refused:
        raise CheckFailed("CLI command: " + str(args[0]))
    return json.loads(result.stdout)


def local(binary, root, endpoint, check):
    notes = root / "notes"
    notes.mkdir()
    for note in (PUMP, RELAY, BRACKET):
        note.write(notes)
    extra = notes / "ignored.csv"
    extra.write_text("count,3\n", encoding="utf-8")
    state = root / "personal"
    cli(binary, state, "init")
    review = cli(binary, state, "ingest", notes, "--review")
    rows = {row["path"]: row["status"] for row in review["source_review"]}
    check("personal_ingest_and_skip", review["updated_sources"] == 3
          and rows[extra.name] == "skipped")
    result = cli(binary, state, "query", QUESTION, "--explain")
    hit, = result["evidence"]
    opened = cli(binary, state, "citation", hit["chunk_id"], "--revision", hit["revision"])
    check("personal_rank_and_exact_revision", result["explanation"]["results"][0]["chunk_id"]
          == hit["chunk_id"] and opened["status"] == "active"
          and opened["evidence"]["text"] == PUMP.text[hit["start"]:hit["end"]]
          and opened["evidence"]["revision"] == hit["revision"] == PUMP.revision)
    if endpoint is None:
        return

    state = root / "advanced"
    cli(binary, state, "init", "--upgrade")
    cli(binary, state, "ingest", notes)

    def graph():
        return by_path(cli(binary, state, "query", QUESTION, "--graph"))

    cli(binary, state, "extract-graph", "--model-endpoint", endpoint)
    unverified = next(row for path, row in graph().items() if Path(path).name == RELAY.path)
    check("model_edge_explicitly_unverified",
          unverified["graph_support"]["assertion_status"] == "extracted_unverified")
    cli(binary, state, "extract-graph", "--source-clauses", "--model-endpoint", endpoint)
    staged = cli(binary, state, "graph-review", "list")["proposals"]
    pending = {Path(row["path"]).name: row["proposal_id"] for row in staged}
    check("local_proposals_stay_outside_graph", set(pending) == {PUMP.path, RELAY.path, BRACKET.path}
          and not any(row.get("graph_support") for row in graph().values()))
    cli(binary, state, "graph-review", "approve", pending[PUMP.path])
    cli(binary, state, "graph-review", "approve", pending[RELAY.path])
    approved = next(row for path, row in graph().items() if Path(path).name == RELAY.path)
    check("local_approval_and_exact_quote",
          approved["graph_support"]["assertion_status"] == "reviewed"
          and approved["graph_support"]["assertion"]["quote"] == RELAY.quote)
    (notes / BRACKET.path).write_text("The older component was removed.\n", encoding="utf-8")
    cli(binary, state, "graph-review", "approve", pending[BRACKET.path], refused=True)
    dismissal = cli(binary, state, "graph-review", "dismiss", pending[BRACKET.path])
    check("local_raw_edit_refused_and_proposal_dismissed", dismissal["status"] == "dismissed"
          and cli(binary, state, "graph-review", "list")["proposals"] == []
          and not any(Path(path).name == BRACKET.path for path in graph()))


def write_policy(fixture, *, reader_allowed):
    active = fixture.policy / "current.json"
    policy = json.loads(active.read_bytes())
    policy["version"] += 1
    policy["collections"]["open"] = {
        "subjects": ["admin", "reader"] if reader_allowed else ["admin"],
        "endpoints": ["extract"],
        "graph_review_subjects": ["reader"],
        "graph_publication": {
            "trusted_paths": ["seed.md", "routine.md", "high.md"],
            "routine_predicates": ["requires"],
            "high_impact_predicates": ["owned_by"],
        },
    }
    data = encoded(policy)
    (fixture.policy / "versions" / f"{policy['version']}.json").write_bytes(data)
    replacement = fixture.policy / "next.json"
    replacement.write_bytes(data)
    os.replace(replacement, active)


def shared(binary, root, openssl, port, check):
    root.mkdir()
    fixture = Fixture(root, openssl, port)
    endpoint = root / "extract.json"
    config = json.loads(endpoint.read_bytes())
    config["predicates"] = ["requires", "owned_by"]
    endpoint.write_bytes(encoded(config))
    write_policy(fixture, reader_allowed=True)

    def declare(name, relation):
        (fixture.open / name).write_text(f"```anyrag-relations\n{relation}\n```\n", encoding="utf-8")

    declare("seed.md", "CedarPump | requires | RelayKit")
    declare("routine.md", "RelayKit | requires | SpareBracket")
    declare("high.md", HIGH)
    PROSE.write(fixture.open)
    service = Service(binary, fixture.profile, fixture.tokens)
    list_route = "/v1/graph-review/list"
    decide_route = "/v1/graph-review/decide"

    def proposals(role="reader"):
        return service.call(list_route, {"collection": "open"}, role)

    def decision(row, choice):
        return service.call(decide_route, {
            "collection": "open", "decision": choice, "idempotency_key": "walkthrough-" + choice,
            **{key: row[key] for key in ("proposal_id", "revision", "fingerprint", "policy_version")},
        })

    try:
        service.start()
        check("shared_requires_signed_identity", service.query(None)[0] == 401
              and service.admin("status")[0] == 200)
        check("shared_sync", service.sync()["status"] == "complete")
        code, result = service.query()
        rows = by_path(result)
        routine = rows["routine.md"]
        reopened_code, reopened = service.cite(routine)
        check("routine_declaration_published_with_citation", code == 200
              and routine["graph_support"]["assertion_status"] == "declared"
              and routine["graph_support"]["assertion"]["quote"] == "RelayKit | requires | SpareBracket"
              and reopened_code == 200 and reopened["evidence"]["revision"] == routine["revision"])
        code, listed = proposals()
        pending = {row["path"]: row for row in listed["proposals"]}
        check("signed_queue_stages_prose_and_high_impact", code == 200
              and set(pending) == {"high.md", PROSE.path}
              and pending["high.md"]["reason"] == "high_impact_predicate"
              and pending[PROSE.path]["reason"] == "model_only"
              and all(path not in rows for path in pending)
              and proposals("admin")[0] == 403)
        accepted = decision(pending["high.md"], "approve")
        dismissed = decision(pending[PROSE.path], "dismiss")
        reviewed = by_path(service.query()[1])
        high = reviewed["high.md"]
        check("signed_review_publishes_only_approved_link", accepted[0] == dismissed[0] == 200
              and decision(pending["high.md"], "approve") == accepted
              and high["graph_support"]["assertion_status"] == "reviewed"
              and high["graph_support"]["assertion"]["quote"] == HIGH
              and PROSE.path not in reviewed and proposals()[1]["proposals"] == [])
        backup_code, backup = service.admin("backup", {})
        check("shared_backup", backup_code == 200)
        write_policy(fixture, reader_allowed=False)
        check("revocation_applies_to_saved_citations_and_queue", service.query()[1]["evidence"] == []
              and service.cite(routine)[0] == 404 and proposals()[0] == 403)
        declare("routine.md", UPDATED)
        (fixture.open / "high.md").unlink()
        check("shared_edit_and_delete_retire_citations", service.sync()["status"] == "complete"
              and service.cite(routine, "admin")[0] == 404 and service.cite(high, "admin")[0] == 404)
        restore_code, _ = service.admin("restore", {"backup_id": backup["backup_id"]})
        check("restore_refuses_reads_until_reconciled", restore_code == 200
              and service.query("admin")[0] == 503 and service.admin("reopen", {})[0] == 409)
        check("restore_reconciles_current_sources_and_grants",
              service.sync()["status"] == "complete" and service.admin("reopen", {})[0] == 200
              and service.cite(routine, "admin")[0] == 404 and service.cite(high, "admin")[0] == 404
              and service.query()[1]["evidence"] == []
              and by_path(service.query("admin")[1])["routine.md"]["graph_support"]["assertion"]["quote"]
              == UPDATED)
    finally:
        service.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True, help="new file for the redacted receipt")
    parser.add_argument("--openssl", default="openssl")
    parser.add_argument("--personal-only", action="store_true", help="run on the standalone build")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    started = time.monotonic()
    receipt = {"schema_version": 1, "fixture": "anyrag-operator-v1",
               "scope": "synthetic reproduction with deterministic loopback responses",
               "binary_sha256": file_sha(binary),
               "runner_sha256": file_sha(Path(__file__)),
               "fixture_helper_sha256": file_sha(Path(__file__).with_name("rehearse_shared.py")),
               "status": "incomplete", "checks": {}}

    def check(name, passed):
        receipt["checks"][name] = "pass" if passed else "fail"
        if not passed:
            raise CheckFailed(name)

    with args.receipt.open("x", encoding="utf-8") as output, tempfile.TemporaryDirectory(
        prefix="anyrag-operator-"
    ) as temp:
        root = Path(temp)
        server = None
        try:
            endpoint = None
            if not args.personal_only:
                server = OperatorModel((PUMP, RELAY, BRACKET, PROSE)).serve()
                endpoint = root / "relations.json"
                endpoint.write_bytes(encoded({
                    "url": f"http://127.0.0.1:{server.server_port}/v1/chat/completions",
                    "model": MODEL, "predicates": ["requires"],
                    "response_format": "schema", "max_tokens": 512,
                }))
            local(binary, root, endpoint, check)
            if server is not None:
                shared(binary, root / "shared", args.openssl, server.server_port, check)
            receipt["status"] = "pass"
        except CheckFailed as error:
            receipt.update(status="fail", failed_check=str(error))
        except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError,
                queue.Empty) as error:
            receipt["driver_error"] = type(error).__name__
        finally:
            if server is not None:
                server.shutdown()
                server.server_close()
            receipt["elapsed_seconds"] = round(time.monotonic() - started, 3)
            output.write(json.dumps(receipt, indent=2) + "\n")
    print(f"Operator rehearsal {receipt['status']}. {len(receipt['checks'])} checks recorded.")
    if receipt["status"] != "pass":
        print(receipt.get("failed_check", receipt.get("driver_error", "interrupted")))
    return 0 if receipt["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
