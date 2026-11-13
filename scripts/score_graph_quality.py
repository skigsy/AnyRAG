"""Score frozen Rust RAG graph receipts against an independently held key."""

from argparse import ArgumentParser
import json
from pathlib import Path
import re
import unicodedata


from rehearse_graph_quality import DEFAULT_FIXTURE, MANIFEST_SHA256, load_fixture, sha

def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def normalized(text):
    value = unicodedata.normalize("NFKC", text).casefold()
    return " ".join(re.sub(r"[^\w]+", " ", value).split())


def checked_evidence(rows, documents):
    if not isinstance(rows, list) or len(rows) > 5:
        raise ValueError("Expected a maximum of five passages")
    for item in rows:
        path = item.get("path")
        if path not in documents:
            raise ValueError("Evidence outside the frozen corpus")
        raw, decoded = documents[path]
        first, last = item.get("start"), item.get("end")
        if (type(first) is not int or type(last) is not int
                or not 0 <= first < last <= len(decoded)
                or item.get("text") != decoded[first:last]
                or item.get("source_hash") != raw):
            raise ValueError("Invalid citation span or source hash")
        support = item.get("graph_support")
        if support is not None:
            assertion = support["assertion"]
            start, end = assertion["start"], assertion["end"]
            if (type(start) is not int or type(end) is not int or not first <= start < end <= last
                    or assertion.get("quote") != decoded[start:end]):
                raise ValueError("Invalid graph assertion anchor")
    return rows


def covers(rows, expected):
    return all(any(hit["path"] == anchor["file"] and anchor["quote"] in hit["text"]
                   for hit in rows) for anchor in expected)


def answer_ok(result, expected, documents):
    if result.get("status") != "answered" or not isinstance(result.get("answer"), str):
        return False
    answer = normalized(result["answer"])
    required = expected["required_facts"]
    forbidden = expected["forbidden_facts"]
    rows = checked_evidence(result["evidence"], documents)
    citations = result["citations"]
    by_id = {row["chunk_id"]: row for row in rows}
    if not isinstance(citations, list) or not citations or any(cite not in by_id for cite in citations):
        raise ValueError("Invalid answer citation IDs")
    return (all(any(normalized(fact) in answer for fact in group) for group in required)
            and all(normalized(fact) not in answer for fact in forbidden)
            and covers([by_id[cite] for cite in citations], expected["evidence"]))


def evaluate(receipt, expected, kinds, documents):
    seen = set()
    counts = dict(off=0, on=0, controls_off=0, controls_on=0)
    misses = []
    regressions = []
    control_regressions = []
    errors = []
    for row in receipt["questions"]:
        qid = row["id"]
        if qid in seen or qid not in expected or row["kind"] != kinds[qid]:
            raise ValueError("Unexpected or repeated question ID or kind")
        seen.add(qid)
        kind = kinds[qid]
        want = expected[qid]
        try:
            on = checked_evidence(row["graph_on"]["result"]["evidence"], documents)
            off = checked_evidence(row["graph_off"]["result"]["evidence"], documents)
            if kind == "two_hop":
                counts["off"] += covers(off, want["evidence"])
                counts["on"] += covers(on, want["evidence"])
                if not covers(on, want["evidence"]):
                    misses.append(qid)
                if covers(off, want["evidence"]) and not covers(on, want["evidence"]):
                    regressions.append(qid)
            elif kind == "negative":
                if row["graph_on"]["result"].get("status") == "evidence":
                    errors.append(f"{qid}: graph returned strong evidence for an absent source")
                negative = row["answer_on"]["result"]
                if negative.get("status") != "insufficient_evidence" or negative.get("citations") != []:
                    errors.append(f"{qid}: graph answer did not abstain")
            else:
                if covers(off, want["evidence"]) and not covers(on, want["evidence"]):
                    control_regressions.append(qid)
                answered_off = answer_ok(row["answer_off"]["result"], want, documents)
                answered_on = answer_ok(row["answer_on"]["result"], want, documents)
                counts["controls_off"] += answered_off
                counts["controls_on"] += answered_on
                if answered_off and not answered_on:
                    control_regressions.append(qid)
                if (row["answer_on"]["result"].get("status") == "answered" and not answered_on
                        and row["answer_off"]["result"].get("status") != "answered"):
                    control_regressions.append(qid)
        except (ValueError, KeyError, TypeError) as exc:
            errors.append(f"{qid}: {exc}")
    if seen != expected.keys():
        errors.append("Incomplete set of questions")
    extraction = receipt["extraction"]
    eligible, completed = extraction["eligible"], extraction["completed"]
    question_count = sum(kind == "two_hop" for kind in kinds.values())
    extraction_ok = eligible > 0 and completed / eligible >= 0.9 and extraction["within_budget"]
    retrieval_ok = (counts["on"] / question_count >= 0.8
                    and (counts["on"] - counts["off"]) / question_count >= 0.25)
    return {"run": receipt["run"], "counts": counts, "two_hop_questions": question_count,
            "graph_on_misses": misses, "graph_on_retrieval_losses": regressions,
            "control_regressions": sorted(set(control_regressions)), "errors": errors,
            "valid_passages": [completed, eligible], "extraction_seconds": extraction["seconds"],
            "extraction_passed": extraction_ok, "retrieval_passed": retrieval_ok,
            "scored_checks_passed": extraction_ok and retrieval_ok and not control_regressions and not errors}


def main():
    parser = ArgumentParser(description=__doc__)
    parser.add_argument("receipts", type=Path, nargs="+")
    parser.add_argument("--fixture", type=Path, default=DEFAULT_FIXTURE)
    parser.add_argument("--answer-key", type=Path, required=True)
    args = parser.parse_args()
    manifest, public = load_fixture(args.fixture)
    if sha(args.answer_key) != manifest["private_key_sha256"]:
        parser.error("The answer key does not match the frozen fixture.")
    documents = {name: (manifest["public"][name], text) for name, text in public.items()
                 if name.startswith("corpus/")}
    expected = {row["id"]: row for row in read_json(args.answer_key)["questions"]}
    kinds = {row["id"]: row["kind"] for row in json.loads(public["questions.json"])["questions"]}
    manifest = MANIFEST_SHA256
    runs = [read_json(path) for path in args.receipts]
    if any(run["fixture_manifest_sha256"].casefold() != manifest for run in runs):
        raise ValueError("A receipt has the wrong frozen fixture digest")
    if len({run["run"] for run in runs}) != len(runs) or len({run["build_sha256"] for run in runs}) != 1:
        raise ValueError("Expected distinct runs with a single frozen executable")
    scores = [evaluate(run, expected, kinds, documents) for run in runs]
    passed = len(scores) == 3 and all(score["scored_checks_passed"] for score in scores)
    print(json.dumps({"schema_version": 1, "fixture_manifest_sha256": manifest, "runs": scores,
                      "three_scored_runs_passed": passed}, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
