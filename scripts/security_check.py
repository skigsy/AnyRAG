"""Scan only approved public files; never send original bundles to a scanner."""

from __future__ import annotations

import argparse
import codecs
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

from check_workspace import ROOT


def history_label_hits(root: Path, terms: set[str]) -> tuple[int, int]:
    """Compare reachable Git objects in memory without emitting private labels."""
    git = ["git", "--no-replace-objects"]
    objects = subprocess.check_output(
        [*git, "rev-list", "--objects", "--all", "--no-object-names"], cwd=root, stderr=subprocess.DEVNULL,
    ).splitlines()
    if not objects:
        raise ValueError("Empty history")
    overlap = max(map(len, terms)) - 1
    hits = 0
    with subprocess.Popen([*git, "cat-file", "--batch"], cwd=root, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as process:
        for object_id in objects:
            process.stdin.write(object_id + b"\n")
            process.stdin.flush()
            header = process.stdout.readline().split()
            if len(header) != 3 or header[0] != object_id:
                raise ValueError("Missing Git object")
            remaining = int(header[2])
            decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
            tail = ""
            matched = False
            while remaining:
                block = process.stdout.read(min(remaining, 256 * 1024))
                if not block:
                    raise ValueError("Incomplete Git object")
                remaining -= len(block)
                text = tail + decoder.decode(block, final=remaining == 0).casefold()
                matched = matched or any(term in text for term in terms)
                tail = text[-overlap:]
            if process.stdout.read(1) != b"\n":
                raise ValueError("Invalid Git object frame")
            hits += matched
        process.stdin.close()
        if process.wait():
            raise ValueError("Git object scan failed")
    return hits, len(objects)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gitleaks", default="gitleaks")
    parser.add_argument("--private-archives", action="store_true", help="Check private labels in memory; never print their values.")
    parser.add_argument("--history", action="store_true")
    parser.add_argument("--work-dir", type=Path)
    args = parser.parse_args()
    boundary = subprocess.run([sys.executable, str(ROOT / "scripts/check_workspace.py")], cwd=ROOT, capture_output=True)
    if boundary.returncode:
        print("FAIL: public file boundary must pass before content scanning.")
        return 1
    manifest = json.loads((ROOT / "public-files.json").read_text(encoding="utf-8"))
    files = [ROOT / name for name in manifest["files"]]
    terms: set[str] = set()
    if args.private_archives:
        for archive in ROOT.glob("*.zip"):
            label = re.split(r"-20\d{6}T", archive.stem, maxsplit=1)[0]
            if len(label) >= 4:
                terms.add(label.casefold())
        if not terms:
            print("FAIL: no private custody labels available for local comparison.")
            return 1
    identifier_hits = 0
    for candidate in files:
        if candidate.suffix.lower() == ".png":
            # Approved artwork is binary. Search any uncompressed text and review pixels separately.
            raw = candidate.read_bytes()
            if not raw.startswith(b"\x89PNG\r\n\x1a\n"):
                print("FAIL: an approved PNG has an invalid signature.")
                return 1
            content = raw.decode("utf-8", errors="replace").casefold()
        else:
            content = candidate.read_text(encoding="utf-8").casefold()
        identifier_hits += sum(content.count(term) for term in terms)
    if identifier_hits:
        print(f"FAIL: {identifier_hits} private-identifier occurrence(s); values withheld.")
        return 1
    if args.work_dir:
        args.work_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rust-rag-public-scan-", dir=args.work_dir) as temporary:
        snapshot = Path(temporary).resolve()
        # This unique directory contains public allowlisted copies only.
        if args.work_dir and snapshot.parent != args.work_dir.resolve():
            raise RuntimeError("unexpected temporary scan path")
        for source in files:
            destination = snapshot / source.relative_to(ROOT)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
        command = [args.gitleaks, "dir", str(snapshot), "--redact", "--no-banner", "--exit-code", "1"]
        try:
            scanned = subprocess.run(command, capture_output=True, text=True, check=False)
        except FileNotFoundError:
            print("FAIL: install the official Gitleaks scanner; scanning was not skipped.")
            return 1
        if scanned.returncode:
            print("FAIL: Gitleaks reported a finding or could not complete; inspect locally with redaction.")
            return 1
    if args.history:
        head = subprocess.run(["git", "rev-parse", "--verify", "HEAD"], cwd=ROOT, capture_output=True, check=False)
        if head.returncode:
            print("FAIL: no public commit exists; an empty history cannot establish history clearance.")
            return 1
        shallow = subprocess.run(["git", "rev-parse", "--is-shallow-repository"], cwd=ROOT, capture_output=True)
        if shallow.returncode or shallow.stdout.strip() != b"false":
            print("FAIL: the history scan needs a complete Git checkout.")
            return 1
        if terms:
            try:
                history_hits, object_count = history_label_hits(ROOT, terms)
            except (OSError, ValueError, subprocess.CalledProcessError):
                print("FAIL: the Git-history private-label scan could not complete.")
                return 1
            if history_hits:
                print(f"FAIL: {history_hits} reachable Git object(s) matched private labels; values withheld.")
                return 1
            print(f"PASS: private-label comparison checked {object_count} reachable Git objects.")
        scanned = subprocess.run(
            [args.gitleaks, "git", str(ROOT), "--log-opts=--all", "--redact", "--no-banner", "--exit-code", "1"],
            capture_output=True, text=True, check=False,
        )
        if scanned.returncode:
            print("FAIL: complete Git-history scan did not pass.")
            return 1
    print(f"PASS: Gitleaks checked {len(files)} approved public files; identifier occurrences: {identifier_hits}.")
    if args.history:
        print("PASS: complete public Git-history Gitleaks scan.")
    print("No original archive contents were copied to the scan snapshot. Manual privacy review is still required.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
