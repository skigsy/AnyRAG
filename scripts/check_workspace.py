"""Narrow planning hygiene checks. This is not a secret scanner or release gate."""

from __future__ import annotations

import argparse
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tomllib
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parent.parent
REQUIRED = {
    "AGENTS.md", "README.md", "SECURITY.md", "docs/project/inspect.md", "docs/project/plan.md", "status.md",
    "docs/architecture.md", "infra/README.md",
    "public-files.json", "scripts/check_workspace.py", ".gitignore", ".dockerignore",
    ".gitattributes", ".github/workflows/validate.yml",
    ".github/pull_request_template.md",
}

FORBIDDEN_SUFFIXES = {
    ".zip", ".7z", ".rar", ".tar", ".gz", ".eml", ".msg", ".pst", ".ost",
    ".db", ".sqlite", ".sqlite3", ".parquet", ".pkl", ".pickle", ".pem",
    ".key", ".pfx", ".p12", ".tfstate", ".tfplan", ".tfvars", ".log",
    ".bak", ".backup",
}
FORBIDDEN_PARTS = {
    ".git", ".aws", ".azure", ".secrets", ".credentials", "node_modules",
    ".venv", "private", "originals", "imports", "quarantine", "backups",
    "data", "documents", "uploads", "indexes", "runtime",
}
VERSION_TAG = r"v\d+\.\d+\.\d+"
RELEASE_LINK = (
    r"https://github\.com/skigsy/AnyRAG/releases/(?:tag|download)/"
    r"(v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?)(?=[/\s)])"
)


def check_release_docs(candidate_tag: str | None) -> list[str]:
    result = subprocess.run(["git", "tag", "--merged", "HEAD", "--list", "v*"], cwd=ROOT,
                            capture_output=True, text=True, check=False)
    if result.returncode:
        return ["Cannot read version tags for the release documentation check."]
    tags = set(result.stdout.splitlines())
    stable = [tag for tag in tags if re.fullmatch(VERSION_TAG, tag)]
    if not stable:
        return ["Fetch version tags before checking the release documentation."]
    latest = max(stable, key=lambda tag: tuple(map(int, tag[1:].split("."))))
    if candidate_tag is not None:
        with (ROOT / "crates/rust-rag/Cargo.toml").open("rb") as crate:
            crate_tag = "v" + tomllib.load(crate)["package"]["version"]
        if not re.fullmatch(VERSION_TAG, candidate_tag) or candidate_tag != crate_tag:
            return ["The proposed tag must match the version in the Rust package."]
        if tuple(map(int, candidate_tag[1:].split("."))) < tuple(map(int, latest[1:].split("."))):
            return ["The proposed tag must be at least the latest reachable stable version."]
        tags.add(candidate_tag)
        latest = candidate_tag
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    status = (ROOT / "status.md").read_text(encoding="utf-8")
    errors = []
    if set(re.findall(RELEASE_LINK, readme)) != {latest}:
        errors.append(f"README release links must all point to the latest stable tag {latest}.")
    documented = set(re.findall(r"^\| \[(v[^\]]+)\]\(", status, re.MULTILINE))
    if missing := tags - documented:
        errors.append("Status table is missing version tags: " + ", ".join(sorted(missing)) + ".")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--require-git", action="store_true")
    parser.add_argument("--release-docs", action="store_true")
    parser.add_argument("--candidate-tag", help="check docs for a prospective release tag")
    args = parser.parse_args()
    if args.candidate_tag and not args.release_docs:
        parser.error("--candidate-tag requires --release-docs")
    errors: list[str] = []
    try:
        manifest = json.loads((ROOT / "public-files.json").read_text(encoding="utf-8"))
        paths = manifest["files"]
        if manifest["schema_version"] != 1 or not isinstance(paths, list):
            raise ValueError("unsupported manifest shape")
        if not all(isinstance(p, str) for p in paths):
            raise ValueError("paths must be strings")
    except (OSError, ValueError, KeyError, TypeError):
        print("FAIL: public-files.json is missing or invalid.")
        return 1
    approved = set(paths)
    if len(paths) != len(approved):
        errors.append("Public manifest contains duplicate entries.")
    if not REQUIRED.issubset(approved):
        errors.append("Required planning files are missing from the public manifest.")
    safe_paths: list[Path] = []
    for name in paths:
        relative = PurePosixPath(name)
        if (
            not name or relative.is_absolute() or ".." in relative.parts
            or "\\" in name or ":" in name or str(relative) != name
            or any(part.lower() in FORBIDDEN_PARTS for part in relative.parts)
            or relative.suffix.lower() in FORBIDDEN_SUFFIXES
            or relative.name.lower() in {"credentials.json", "secrets.json"}
            or relative.name.lower().endswith(".tfvars.json")
            or any(s.lower().startswith(".sqlite") or s.lower() == ".tfstate" for s in relative.suffixes)
            or any(part.lower().startswith(".env") and part != ".env.example" for part in relative.parts)
        ):
            errors.append("A public manifest entry violates the path policy.")
            continue
        candidate = ROOT.joinpath(*relative.parts)
        # Reject symlinks/junctions in any component, and never read outside ROOT.
        components = [ROOT.joinpath(*relative.parts[:i]) for i in range(1, len(relative.parts) + 1)]
        if any(p.is_symlink() or p.is_junction() for p in components):
            errors.append("A public manifest entry traverses a link.")
        elif not candidate.is_file() or not candidate.resolve().is_relative_to(ROOT):
            errors.append("A public manifest entry is missing or outside the workspace.")
        elif candidate.stat().st_size > 1024 * 1024:
            errors.append("A public planning file exceeds the 1 MiB review limit.")
        else:
            safe_paths.append(candidate)
    for candidate in safe_paths:
        if candidate.suffix != ".md":
            continue
        content = candidate.read_text(encoding="utf-8")
        for target in re.findall(r"\[[^\]]*\]\(([^)\s]+)\)", content):
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc or not parsed.path:
                continue
            resolved = (candidate.parent / unquote(parsed.path)).resolve()
            if not resolved.is_relative_to(ROOT) or not resolved.is_file():
                errors.append("A Markdown file contains an invalid local link.")
            elif resolved.relative_to(ROOT).as_posix() not in approved:
                errors.append("A Markdown link points outside the public manifest.")
    try:
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=ROOT,
            capture_output=True, text=True, check=False,
        )
        has_git = top.returncode == 0 and Path(top.stdout.strip()).resolve() == ROOT
    except FileNotFoundError:
        has_git = False
    if has_git:
        if args.release_docs:
            errors.extend(check_release_docs(args.candidate_tag))
        result = subprocess.run(
            ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=False,
        )
        if result.returncode:
            errors.append("Cannot inspect the Git index.")
        else:
            tracked = set(result.stdout.decode("utf-8", errors="surrogateescape").split("\0")) - {""}
            if tracked - approved:
                errors.append(f"{len(tracked - approved)} tracked path(s) are outside the public manifest.")
            if args.require_git and approved - tracked:
                errors.append("Some public manifest files are absent from the Git index.")
    elif args.require_git or args.release_docs:
        errors.append("A Git repository rooted at this workspace is required.")
    else:
        print("NOTE: No workspace Git repository; index boundary was not checked.")
    if errors:
        for message in sorted(set(errors)):
            print(f"FAIL: {message}")
        return 1
    print(f"PASS: planning hygiene for {len(approved)} public files.")
    print("Archive contents, secret scanning, Git history, and runtime are NOT certified.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
