"""Install a checksum-pinned official scanner into an explicit tools directory."""

from __future__ import annotations

import argparse
import hashlib
import io
from pathlib import Path
import platform
import tarfile
import urllib.request
import zipfile


VERSION = "8.30.1"
ASSETS = {
    "Windows": ("windows_x64.zip", "d29144deff3a68aa93ced33dddf84b7fdc26070add4aa0f4513094c8332afc4e", "gitleaks.exe"),
    "Linux": ("linux_x64.tar.gz", "551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb", "gitleaks"),
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, required=True)
    args = parser.parse_args()
    if platform.machine().lower() not in {"amd64", "x86_64"} or platform.system() not in ASSETS:
        parser.error("This maintainer bootstrap supports Windows/Linux x64; install official Gitleaks separately elsewhere.")
    suffix, expected, binary = ASSETS[platform.system()]
    url = f"https://github.com/gitleaks/gitleaks/releases/download/v{VERSION}/gitleaks_{VERSION}_{suffix}"
    request = urllib.request.Request(url, headers={"User-Agent": "Rust-RAG-scanner-bootstrap"})
    with urllib.request.urlopen(request, timeout=60) as response:
        payload = response.read(32 * 1024 * 1024)
    if hashlib.sha256(payload).hexdigest() != expected:
        raise RuntimeError("Official scanner archive checksum mismatch; installation stopped.")
    if suffix.endswith(".zip"):
        with zipfile.ZipFile(io.BytesIO(payload)) as archive:
            content = archive.read(binary)
    else:
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            stream = archive.extractfile(binary)
            if stream is None:
                raise RuntimeError("Scanner binary absent from official archive.")
            content = stream.read()
    args.target_dir.mkdir(parents=True, exist_ok=True)
    target = args.target_dir / binary
    target.write_bytes(content)
    target.chmod(0o755)
    print(f"Installed checksum-verified Gitleaks {VERSION}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
