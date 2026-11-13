"""Four small, model-free smoke tests against the compiled Any RAG CLI."""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from uuid import uuid4
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]


def cli(binary, state, *args, ok=True):
    result = subprocess.run([binary, "--state-dir", state, "--json", *args],
                            capture_output=True, check=False, timeout=5)
    assert (result.returncode == 0) == ok, result.stderr.decode()
    return json.loads(result.stdout)


def setup(binary, root, *, advanced=False):
    source = root / "source"
    source.mkdir()
    state = root / "index"
    cli(binary, state, "init", *(["--upgrade"] if advanced else []))
    return state, source


def first_hit(binary, state, question):
    return cli(binary, state, "query", question)["evidence"][0]


def mcp(binary, root):
    state, source = setup(binary, root)
    note = source / "pump.md"
    note.write_text("The azurite pump needs a spare relay.")
    cli(binary, state, "ingest", source)
    hit = first_hit(binary, state, "azurite")
    with subprocess.Popen([binary, "--state-dir", state, "mcp"], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, text=True, encoding="utf-8") as server:
        def send(method, params=None, id=None):
            msg = {"jsonrpc": "2.0", "method": method}
            if params is not None:
                msg["params"] = params
            if id is not None:
                msg["id"] = id
            server.stdin.write(json.dumps(msg) + "\n")
            server.stdin.flush()
            if id is not None:
                return json.loads(server.stdout.readline())["result"]

        try:
            send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                "clientInfo": {"name": "smoke", "version": "1"}}, 1)
            send("notifications/initialized")
            assert {t["name"] for t in send("tools/list", id=2)["tools"]} == {"search", "open_citation"}
            args = {"name": "open_citation", "arguments": {k: hit[k] for k in ("chunk_id", "revision")}}
            opened = send("tools/call", args, 3)["structuredContent"]["evidence"]
            assert (opened["text"], opened["revision"]) == (hit["text"], hit["revision"])
            note.unlink()
            cli(binary, state, "ingest", source, "--sync")
            assert send("tools/call", args, 4)["isError"] is True
        finally:
            server.kill()


def dashboard(binary, root):
    import pty
    import select
    from signal import SIGTERM

    state, source = setup(binary, root)
    note = source / "pump.md"
    note.write_text("Azurite: <fixture> evidence.")
    (source / "notes.csv").write_text("unsupported,fixture")
    report = cli(binary, state, "ingest", source, "--review")
    assert any(row["path"] == "notes.csv" and row["status"] == "skipped"
               for row in report["source_review"])
    pid, terminal = pty.fork()
    if pid == 0:
        os.execl(binary, binary, "--state-dir", state, "dashboard", "--no-open")
    try:
        startup = b""
        deadline = time.monotonic() + 3
        while b"Press Ctrl+C" not in startup and time.monotonic() < deadline:
            if select.select([terminal], [], [], max(0, deadline - time.monotonic()))[0]:
                startup += os.read(terminal, 4096)
        origin, code = re.search(rb"Dashboard: (http://[^\s]+)/\r?\nDashboard access code: ([a-f0-9]{64})",
                                 startup).groups()
        origin = origin.decode()
        headers = {"Authorization": "Bearer " + code.decode()}

        def read(path, *, authenticated=True):
            request = urllib.request.Request(origin + path, headers=headers if authenticated else {})
            with urllib.request.urlopen(request, timeout=2) as reply:
                return reply.read().decode()

        assert "ingest-review-file" in read("/")
        try:
            read("/sources", authenticated=False)
        except urllib.error.HTTPError as error:
            assert error.code == 401
        else:
            raise AssertionError("Index contents were available without pairing")
        source_list = read("/sources")
        source_id = re.search(r'data-source-id="([a-f0-9]{64})"', source_list)[1]
        assert "pump.md" in source_list and "notes.csv" not in source_list
        page = read("/sources/" + source_id)
        assert "Offsets" in page and "&lt;fixture&gt;" in page and "<fixture>" not in page
        note.unlink()
        cli(binary, state, "ingest", source, "--sync")
        assert "no longer in the index" in read("/sources/" + source_id)
    finally:
        os.kill(pid, SIGTERM)
        os.waitpid(pid, 0)
        os.close(terminal)


def scheduled_sync(binary, root):
    state, source = setup(binary, root)
    old = source / "pump.md"
    old.write_text("Azurite pump.")
    cli(binary, state, "ingest", source)
    old.unlink()
    (source / "valve.md").write_text("Beryl valve.")
    service = (ROOT / "infra/anyrag-folder-sync.service").read_text()
    timer = (ROOT / "infra/anyrag-folder-sync.timer").read_text()
    assert "Persistent=true" in timer and "OnCalendar=*:0/15" in timer
    template = shlex.split(next(line.removeprefix("ExecStart=") for line in service.splitlines()
                                if line.startswith("ExecStart=")))
    assert template[:4] == ["%h/.local/bin/rust-rag", "--state-dir",
                            "%h/.local/state/anyrag/index", "ingest"]
    assert template[4] == "%h/Documents/AnyRAG Sources"
    unit = f"anyrag-smoke-{uuid4().hex}"
    try:
        subprocess.run(["systemd-run", "--quiet", f"--unit={unit}", "--on-active=50ms",
                        "--timer-property=AccuracySec=1ms", "--property=Type=oneshot",
                        "--property=StandardOutput=null", binary, "--state-dir", state,
                        "ingest", source, *template[5:]], check=True, timeout=3)
        deadline = time.monotonic() + 3
        while not cli(binary, state, "query", "beryl")["evidence"]:
            assert time.monotonic() < deadline, "The timer did not reconcile the source"
            time.sleep(.02)
        assert cli(binary, state, "query", "azurite")["evidence"] == []
    finally:
        subprocess.run(["systemctl", "stop", unit + ".timer", unit + ".service"],
                       capture_output=True, timeout=3)


def xlsx(binary, root):
    state, source = setup(binary, root, advanced=True)
    workbook = source / "parts.xlsx"
    main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    rels = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    package = "http://schemas.openxmlformats.org/package/2006/relationships"
    content = "http://schemas.openxmlformats.org/package/2006/content-types"
    with ZipFile(workbook, "w") as file:
        file.writestr("[Content_Types].xml", f'<Types xmlns="{content}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
        file.writestr("_rels/.rels", f'<Relationships xmlns="{package}"><Relationship Id="rId1" Type="{rels}/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        file.writestr("xl/workbook.xml", f'<workbook xmlns="{main}" xmlns:r="{rels}"><sheets><sheet name="Inventory" sheetId="1" r:id="rId1"/></sheets></workbook>')
        file.writestr("xl/_rels/workbook.xml.rels", f'<Relationships xmlns="{package}"><Relationship Id="rId1" Type="{rels}/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
        file.writestr("xl/worksheets/sheet1.xml", f'<worksheet xmlns="{main}"><sheetData><row r="7"><c r="C7" t="inlineStr"><is><t>Azurite relay</t></is></c><c r="D7" t="str"><f>LOWER("canaryformula")</f><v>canaryformula</v></c></row></sheetData></worksheet>')
    report = cli(binary, state, "ingest", source, "--xlsx", "--sync")
    assert report["updated_sources"] == 1 and report["failures"] == [], report
    hit = first_hit(binary, state, "azurite")
    assert hit["anchor"] == "sheet:Inventory!C7" and hit["text"] == "Azurite relay", hit
    assert cli(binary, state, "query", "canaryformula")["evidence"] == []
    args = ("citation", hit["chunk_id"], "--revision", hit["revision"])
    assert cli(binary, state, *args)["evidence"]["anchor"] == hit["anchor"]
    workbook.unlink()
    cli(binary, state, "ingest", source, "--xlsx", "--sync")
    assert cli(binary, state, *args, ok=False)["status"] == "error"


if __name__ == "__main__":
    cases = {"mcp": mcp, "dashboard": dashboard, "scheduled-sync": scheduled_sync, "xlsx": xlsx}
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("cases", choices=cases, nargs="+")
    args = parser.parse_args()
    for name in args.cases:
        start = time.monotonic()
        with tempfile.TemporaryDirectory(prefix="anyrag-smoke-") as directory:
            cases[name](args.binary.resolve(), Path(directory))
        print(f"{name}: passed in {(time.monotonic() - start) * 1000:.0f} ms", flush=True)
