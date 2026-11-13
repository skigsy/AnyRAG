#!/usr/bin/env python3
"""Check every crates.io version in Cargo.lock against the OSV advisory service."""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path
from urllib.request import Request, urlopen


OSV = "https://api.osv.dev/v1"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
BATCH_SIZE = 100
MAX_RESPONSE = 2 * 1024 * 1024
# The only accepted advisory is an informational maintenance notice for this
# exact locked crate. Reject it if OSV later reports severity or a different type.
PASTE_NOTICE = ("paste", "1.0.15", "RUSTSEC-2024-0436")


def locked_crates(path):
    with path.open("rb") as stream:
        packages = tomllib.load(stream).get("package")
    if not isinstance(packages, list) or not packages:
        raise ValueError("Cargo.lock has no package list")
    crates = set()
    for package in packages:
        source = package.get("source")
        if source is None:
            continue  # Local workspace packages have no registry source.
        if source != CRATES_IO:
            raise ValueError("Cargo.lock contains a source outside the crates.io audit")
        name, version = package.get("name"), package.get("version")
        if (not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", name)
                or not isinstance(version, str)
                or not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]+)?", version)):
            raise ValueError("Cargo.lock contains an invalid crate name or version")
        crates.add((name, version))
    if not crates:
        raise ValueError("Cargo.lock has no crates.io dependencies")
    return sorted(crates)


def request_json(path, payload=None):
    body = json.dumps(payload).encode("utf-8") if payload is not None else None
    request = Request(f"{OSV}/{path}", data=body, headers={"Content-Type": "application/json"})
    with urlopen(request, timeout=30) as response:
        data = response.read(MAX_RESPONSE + 1)
    if len(data) > MAX_RESPONSE:
        raise ValueError("OSV response exceeded the audit size limit")
    result = json.loads(data)
    if not isinstance(result, dict):
        raise ValueError("OSV response is not an object")
    return result


def batch_advisories(crates):
    queries = [{"package": {"name": name, "ecosystem": "crates.io"}, "version": version}
               for name, version in crates]
    results = request_json("querybatch", {"queries": queries}).get("results")
    if not isinstance(results, list) or len(results) != len(crates):
        raise ValueError("OSV returned an incomplete batch")
    for (name, version), result in zip(crates, results, strict=True):
        if not isinstance(result, dict) or result.keys() - {"vulns", "next_page_token"}:
            raise ValueError("OSV returned an unrecognized query result")
        if result.get("next_page_token"):
            raise ValueError("OSV paginated a batch; a partial audit cannot pass")
        vulnerabilities = result.get("vulns", [])
        if not isinstance(vulnerabilities, list):
            raise ValueError("OSV returned an invalid advisory list")
        for item in vulnerabilities:
            advisory = item.get("id") if isinstance(item, dict) else None
            if not isinstance(advisory, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", advisory):
                raise ValueError("OSV returned an invalid advisory identifier")
            yield name, version, advisory


def accepted_maintenance_notice(finding):
    if finding != PASTE_NOTICE:
        return False
    advisory_id = finding[2]
    record = request_json(f"vulns/{advisory_id}")
    affected = record.get("affected")
    if record.get("id") != advisory_id or record.get("severity") or not isinstance(affected, list):
        return False
    return any(
        isinstance(entry, dict)
        and isinstance(entry.get("package"), dict)
        and entry["package"].get("name") == finding[0]
        and entry["package"].get("ecosystem") == "crates.io"
        and isinstance(entry.get("database_specific"), dict)
        and entry["database_specific"].get("informational") == "unmaintained"
        and not entry["database_specific"].get("cvss")
        and not entry.get("severity")
        for entry in affected
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lockfile", type=Path,
                        default=Path(__file__).resolve().parents[1] / "Cargo.lock")
    args = parser.parse_args()
    try:
        crates = locked_crates(args.lockfile)
        findings = set()
        for start in range(0, len(crates), BATCH_SIZE):
            findings.update(batch_advisories(crates[start:start + BATCH_SIZE]))
        rejected = []
        for name, version, advisory_id in sorted(findings):
            finding = name, version, advisory_id
            if accepted_maintenance_notice(finding):
                print(f"Accepted maintenance notice {advisory_id} for {name}@{version}")
            else:
                rejected.append(f"{name}@{version}: {advisory_id}")
    except (OSError, ValueError) as exc:
        parser.exit(1, f"Cargo advisory check could not complete: {exc}\n")
    if rejected:
        print("Locked crates with unresolved advisories:", file=sys.stderr)
        for item in rejected:
            print(f"  {item}", file=sys.stderr)
        return 1
    print(f"Checked {len(crates)} exact crates.io versions against OSV.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
