"""Build and package a standalone Rust RAG candidate for Windows x64."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import stat
import subprocess
from tempfile import TemporaryDirectory
import tomllib
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


ROOT = Path(__file__).resolve().parent.parent
REPO = "https://github.com/skigsy/AnyRAG"
PACKAGE = "rust-rag"
TARGET = "x86_64-pc-windows-msvc"
DISTRIBUTION_FEATURES = ["dashboard"]
EXE = PACKAGE + ".exe"
CRATE_SOURCE = f"crates/{PACKAGE}"
BUILDER = "scripts/build_native_release.py"
SOURCES = (
    ".cargo/config.toml", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "LICENSE",
    f"{CRATE_SOURCE}/Cargo.toml", f"{CRATE_SOURCE}/src",
)
RUST_FLAGS = ("-C", "target-feature=+crt-static", "-C", "link-arg=/Brepro")
PERMISSIVE = {
    "MIT", "MIT OR Apache-2.0", "Apache-2.0 OR MIT", "Unlicense OR MIT",
    "(MIT OR Apache-2.0) AND Unicode-3.0", "BSD-3-Clause",
}
SYNC_NAMES = {"onedrive", "dropbox", "googledrive", "google drive", "iclouddrive"}


def checked_output(value: Path) -> Path:
    output = value.resolve()
    if value.is_symlink() or output.is_relative_to(ROOT) or output.exists() or not output.parent.is_dir():
        raise ValueError("Choose a new output directory outside the checkout with an existing parent.")
    if any(part.casefold() in SYNC_NAMES or part.casefold().startswith("onedrive -")
           for part in (*value.absolute().parts, *output.parts)):
        raise ValueError("Choose a location outside sync folders.")
    for key in ("OneDrive", "OneDriveConsumer", "OneDriveCommercial", "DROPBOX"):
        if os.getenv(key) and output.is_relative_to(Path(os.environ[key]).resolve()):
            raise ValueError("Choose a location outside sync folders.")
    return output


def capture(args: list[str], cwd: Path, env: dict[str, str] | None = None) -> bytes:
    return subprocess.check_output(args, cwd=cwd, env=env)


def source_snapshot() -> tuple[str, dict[str, bytes]]:
    sha = capture(["git", "rev-parse", "--verify", "HEAD"], ROOT).decode().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("Expected a full Git commit ID.")
    builder = capture(["git", "show", f"{sha}:{BUILDER}"], ROOT)
    if builder.replace(b"\r\n", b"\n") != Path(__file__).read_bytes().replace(b"\r\n", b"\n"):
        raise ValueError("Run the candidate builder from its committed source.")
    public = json.loads(capture(["git", "show", f"{sha}:public-files.json"], ROOT))["files"]
    archive = capture(["git", "archive", "--format=zip", sha, "--", *SOURCES], ROOT)
    result = {}
    with ZipFile(io.BytesIO(archive)) as source:
        for item in source.infolist():
            if item.is_dir():
                continue
            path = PurePosixPath(item.filename)
            if (item.filename not in public or path.is_absolute() or ".." in path.parts
                    or "\\" in item.filename or ":" in item.filename
                    or stat.S_ISLNK(item.external_attr >> 16)):
                raise ValueError("Unexpected member in the native build snapshot.")
            result[item.filename] = source.read(item)
    return sha, result


def write_sources(directory: Path, files: dict[str, bytes]) -> None:
    for name, content in files.items():
        path = directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def json_bytes(value: dict) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def cargo_graph(metadata: dict) -> tuple[dict, list[dict], dict[str, set[str]]]:
    packages = {package["id"]: package for package in metadata["packages"]}
    roots = [
        packages[key] for key in metadata["workspace_members"]
        if packages[key]["name"] == PACKAGE
    ]
    if len(roots) != 1 or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", roots[0]["version"]):
        raise ValueError("Expected one versioned native application in the workspace.")
    root = roots[0]
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [root["id"]]
    edges = {}
    while pending:
        key = pending.pop()
        if key in edges:
            continue
        children = {
            dependency["pkg"] for dependency in nodes[key]["deps"]
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"])
        }
        edges[key] = children
        pending.extend(children)
    dependencies = [packages[key] for key in edges if key != root["id"]]
    return root, sorted(dependencies, key=lambda p: (p["name"], p["version"])), edges


def build_binary(source: Path, target: Path, cargo_home: Path) -> bytes:
    environment = dict(os.environ)
    # These flags take precedence over environment RUSTFLAGS and the source Cargo config.
    remap = (
        f"--remap-path-prefix={cargo_home}=cargo-home",
        f"--remap-path-prefix={source}=rust-rag",
        f"--remap-path-prefix={target}=rust-rag-target",
    )
    environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join((*RUST_FLAGS, *remap))
    environment["CARGO_INCREMENTAL"] = "0"
    environment["CARGO_HOME"] = str(cargo_home)
    environment["RUSTC_WRAPPER"] = ""
    environment["RUSTC_WORKSPACE_WRAPPER"] = ""
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "--offline", "--target", TARGET,
         "--target-dir", str(target), "--package", PACKAGE, "--bin", PACKAGE,
         "--no-default-features", "--features", ",".join(DISTRIBUTION_FEATURES)],
        cwd=source, env=environment, check=True,
    )
    return (target / TARGET / "release" / EXE).read_bytes()


def check_private_paths(binary: bytes, paths: tuple[Path, ...]) -> None:
    lower = binary.lower()
    for path in paths:
        for spelling in {str(path), path.as_posix()}:
            if len(spelling) > 3 and any(
                spelling.lower().encode(encoding) in lower for encoding in ("utf-8", "utf-16-le")
            ):
                raise ValueError("A host-specific build path is embedded in the native binary.")
    if re.search(rb"[a-z]:[\\/]users[\\/]", lower):
        raise ValueError("A Windows user profile path is embedded in the native binary.")


def notices_and_sbom(
    metadata: dict, files: dict[str, bytes], compiler: str, sysroot: Path,
) -> tuple[dict, dict[str, bytes], dict]:
    root, dependencies, edges = cargo_graph(metadata)
    lock = tomllib.loads(files["Cargo.lock"].decode())
    checksums = {(p["name"], p["version"], p.get("source")): p.get("checksum") for p in lock["package"]}
    notices = {"LICENSE": files["LICENSE"]}
    notice_lines = [
        "Rust RAG third-party notices", "",
        "The Cargo build and runtime dependencies below retain their published licenses.",
        "See the corresponding directories in third-party for the license texts.", "",
    ]
    refs = {root["id"]: f"{PACKAGE}@{root['version']}"}
    components = []
    sqlite_ref = "pkg:generic/sqlite"
    for package in dependencies:
        name, version, source = package["name"], package["version"], package["source"]
        checksum = checksums.get((name, version, source))
        license_id = (package["license"] or "").replace("/", " OR ")
        if (source != "registry+https://github.com/rust-lang/crates.io-index"
                or not checksum or not re.fullmatch(r"[0-9a-f]{64}", checksum)
                or license_id not in PERMISSIVE):
            raise ValueError(f"Dependency {name} needs source and license review.")
        crate = Path(package["manifest_path"]).parent
        texts = sorted(
            path for path in crate.iterdir()
            if path.is_file() and re.match(r"(?i)(license|copying|notice|copyright)([._-]|$)", path.name)
        )
        if not texts or ("Unicode-3.0" in license_id and not (crate / "LICENSE-UNICODE").is_file()):
            raise ValueError(f"License texts missing for {name} {version}.")
        for path in texts:
            notices[f"third-party/{name}-{version}/{path.name}"] = path.read_bytes()
        notice_lines.append(f"{name} {version}  {license_id}")
        ref = f"pkg:cargo/{name}@{version}"
        refs[package["id"]] = ref
        components.append({
            "bom-ref": ref, "type": "library", "name": name, "version": version,
            "purl": ref, "licenses": [{"expression": license_id}],
            "hashes": [{"alg": "SHA-256", "content": checksum}],
        })
        if name == "libsqlite3-sys":
            sqlite = (crate / "sqlite3" / "sqlite3.c").read_bytes()
            match = re.search(rb'^#define SQLITE_VERSION\s+"(\d+\.\d+\.\d+)"', sqlite, re.MULTILINE)
            if match is None:
                raise ValueError("Could not identify the bundled SQLite amalgamation.")
            sqlite_version = match[1].decode()
            sqlite_ref = f"pkg:generic/sqlite@{sqlite_version}"
            components.append({
                "bom-ref": sqlite_ref, "type": "library", "name": "SQLite", "version": sqlite_version,
                "purl": sqlite_ref, "licenses": [{"license": {"name": "Public domain"}}],
                "hashes": [{"alg": "SHA3-256", "content": hashlib.sha3_256(sqlite).hexdigest()}],
            })
    if sqlite_ref == "pkg:generic/sqlite":
        raise ValueError("Expected bundled SQLite in the native dependency graph.")

    std_ref = "rust-std"
    notices["third-party/rust-std/COPYRIGHT-library.html"] = (
        sysroot / "share/doc/rust/COPYRIGHT-library.html"
    ).read_bytes()
    for license_file in ("Apache-2.0.txt", "MIT.txt"):
        notices[f"third-party/rust-std/{license_file}"] = (
            sysroot / "share/doc/rust/licenses" / license_file
        ).read_bytes()
    components.append({
        "bom-ref": std_ref, "type": "library", "name": "Rust standard library",
        "version": compiler.split()[1], "licenses": [{"expression": "MIT OR Apache-2.0"}],
    })
    notice_lines += [
        "", f"SQLite {sqlite_version} is in the public domain. https://sqlite.org/copyright.html",
        "The Rust standard library copyright inventory is included under third-party/rust-std.",
        "The executable statically links the Microsoft C runtime through the MSVC toolchain.",
    ]
    notices["THIRD-PARTY-NOTICES.txt"] = ("\n".join(notice_lines) + "\n").encode()
    dependency_graph = []
    for key, children in edges.items():
        links = {refs[child] for child in children}
        if key == root["id"]:
            links.add(std_ref)
        elif refs[key].startswith("pkg:cargo/libsqlite3-sys@"):
            links.add(sqlite_ref)
        dependency_graph.append({"ref": refs[key], "dependsOn": sorted(links)})
    dependency_graph += [{"ref": ref, "dependsOn": []} for ref in (std_ref, sqlite_ref)]
    sbom = {
        "bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
        "metadata": {"component": {
            "bom-ref": refs[root["id"]], "type": "application", "name": PACKAGE,
            "version": root["version"], "licenses": [{"expression": "MIT"}],
        }},
        "components": sorted(components, key=lambda item: item["bom-ref"]),
        "dependencies": sorted(dependency_graph, key=lambda item: item["ref"]),
    }
    return root, notices, sbom


def package(output: Path) -> dict:
    sha, sources = source_snapshot()
    cargo_home = Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo").resolve()
    with TemporaryDirectory(prefix="rust-rag-build-", dir=output.parent) as scratch:
        work = Path(scratch).resolve()
        if work.parent != output.parent:
            raise ValueError("Unexpected build scratch path.")
        source_a, source_b = work / "source-a", work / "source-b"
        for source in (source_a, source_b):
            write_sources(source, sources)
        compiler = capture(["rustc", "--version"], source_a).decode().strip()
        cargo = capture(["cargo", "--version"], source_a).decode().strip()
        toolchain = tomllib.loads(sources["rust-toolchain.toml"].decode())["toolchain"]["channel"]
        if (not re.fullmatch(r"\d+\.\d+\.\d+", toolchain)
                or not compiler.startswith(f"rustc {toolchain} ")
                or not cargo.startswith(f"cargo {toolchain} ")):
            raise ValueError("The actual compiler and Cargo must match the pinned Rust toolchain.")
        sysroot = Path(capture(["rustc", "--print", "sysroot"], source_a).decode().strip()).resolve()
        environment = {**os.environ, "CARGO_HOME": str(cargo_home)}
        metadata = json.loads(capture(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET,
             "--no-default-features", "--features", ",".join(DISTRIBUTION_FEATURES)],
            source_a, environment,
        ))
        root, members, sbom = notices_and_sbom(metadata, sources, compiler, sysroot)
        first = build_binary(source_a, work / "target-a", cargo_home)
        second = build_binary(source_b, work / "target-b", cargo_home)
        if first != second:
            raise ValueError("Native binaries differ across the two fresh build trees.")
        check_private_paths(first, (ROOT, Path.home(), cargo_home, work, sysroot))
        provenance = {
            "schema_version": 1, "source_repository": REPO, "source_commit": sha,
            "package": PACKAGE, "product": PACKAGE, "features": DISTRIBUTION_FEATURES,
            "version": root["version"], "target": TARGET,
            "rustc": compiler, "cargo": cargo, "cargo_lock_sha256": sha256(sources["Cargo.lock"]),
            "executable": EXE, "executable_sha256": sha256(first), "executable_bytes": len(first),
            "independent_build_trees": 2, "byte_identical": True,
            "provenance_type": "unsigned build receipt", "runtime_qualification": "separate check required",
        }
        for variable, field in (("VCToolsVersion", "msvc_toolset"), ("WindowsSDKVersion", "windows_sdk"),
                                ("ImageVersion", "runner_image_version")):
            version = os.environ.get(variable, "").strip("\\/")
            if re.fullmatch(r"\d+(?:\.\d+)+", version):
                provenance[field] = version
        members.update({EXE: first, "sbom.cdx.json": json_bytes(sbom),
                        "build-provenance.json": json_bytes(provenance)})
        candidate = work / "candidate"
        candidate.mkdir()
        (candidate / EXE).write_bytes(first)
        archive = candidate / f"{PACKAGE}-{root['version']}-windows-x64.zip"
        with ZipFile(archive, "w", compression=ZIP_DEFLATED, compresslevel=9) as release:
            for name, data in sorted(members.items()):
                entry = ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                release.writestr(entry, data, compress_type=ZIP_DEFLATED, compresslevel=9)
        archive_sha256 = sha256(archive.read_bytes())
        (candidate / "SHA256SUMS").write_bytes(
            f"{archive_sha256}  {archive.name}\n{sha256(first)}  {EXE}\n".encode()
        )
        candidate.rename(output)
    return {**provenance, "archive": archive.name, "archive_sha256": archive_sha256,
            "sbom_components": len(sbom["components"])}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    if os.name != "nt" or platform.machine().casefold() not in {"amd64", "x86_64"}:
        parser.error("Build this candidate on an x64 Windows host with the MSVC build tools.")
    try:
        output = checked_output(args.output_dir)
        print(json_bytes(package(output)).decode(), end="")
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"Native candidate build failed: {exc}\n")


if __name__ == "__main__":
    main()
