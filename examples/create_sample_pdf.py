"""Create the invented, deterministic, ASCII-only PDF example with the stdlib.

Run from the checkout: python examples/create_sample_pdf.py
No source assets, private data, compressed streams or machine metadata are used.
"""

from __future__ import annotations

import argparse
from pathlib import Path


TITLE = "Synthetic repair manual"
PASSAGES = (
    "This public example was invented for Rust RAG.",
    "Synthetic equipment example; not operational repair advice.",
    "The pretend Cedar repair bench services a demonstration water pump.",
    "The spare filter for the demonstration pump is stored in cabinet nine.",
    "The fictional technician checks the demonstration pump each Thursday.",
    "These statements are test material, not instructions for real machinery.",
)


def _literal(text: str) -> str:
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def build_sample_pdf() -> bytes:
    """Return one page with fixed metadata, correct byte offsets and a text layer."""
    commands = ["BT", "/F1 18 Tf", "48 744 Td", f"({_literal(TITLE)}) Tj", "/F1 11 Tf"]
    for number, passage in enumerate(PASSAGES):
        commands.extend(["0 -32 Td" if number == 0 else "0 -24 Td", f"({_literal(passage)}) Tj"])
    commands.extend(["0 -48 Td", "(1) Tj", "ET", ""])
    content = "\n".join(commands).encode("ascii")
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        f"<< /Length {len(content)} >>\nstream\n".encode("ascii") + content + b"endstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        b"<< /Title (Synthetic repair manual) /Subject (Entirely invented public example) "
        b"/Creator (Rust RAG sample generator) >>",
    ]
    document = bytearray(b"%PDF-1.4\n% ASCII synthetic example; no private source material.\n")
    offsets = [0]
    for number, body in enumerate(objects, 1):
        offsets.append(len(document))
        document.extend(f"{number} 0 obj\n".encode("ascii") + body + b"\nendobj\n")
    cross_reference = len(document)
    document.extend(f"xref\n0 {len(objects) + 1}\n".encode("ascii"))
    document.extend(b"0000000000 65535 f \n")
    for offset in offsets[1:]:
        document.extend(f"{offset:010d} 00000 n \n".encode("ascii"))
    document.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R /Info 6 0 R >>\n"
        f"startxref\n{cross_reference}\n%%EOF\n".encode("ascii")
    )
    result = bytes(document)
    assert result.isascii()
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parent / "library" / "sample.pdf")
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(build_sample_pdf())


if __name__ == "__main__":
    main()
