"""Bounded PDF extraction for the isolated Linux OCR worker."""

import argparse
from bisect import bisect_right
from collections import Counter, defaultdict
from contextlib import redirect_stdout
import csv
from dataclasses import dataclass
from decimal import Decimal
from io import BytesIO, StringIO
import json
import logging
import math
import os
from pathlib import Path
import re
import resource
from statistics import median
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

from PIL import Image
from pypdf import PdfReader


MIB = 1024 * 1024
MAX_INPUT = 16 * MIB
MAX_OUTPUT = 12 * MIB
MAX_PAGE_PIXELS = 6_000_000
MAX_WORDS = 8192
MAX_REGIONS = 32_768
MAX_CHARACTERS = 2_000_000
MAX_OPTICAL_PROBES = 4
RAIL_TOLERANCE = 3
SCALE = 1_000_000
MEASUREMENT_UNITS = {"mm", "cm", "km", "mg", "kg", "ml", "ms"}
Box = tuple[float, float, float, float]


class ExtractionError(Exception):
    pass


@dataclass(frozen=True)
class Word:
    text: str
    box: Box
    source_line: tuple[int, int, int] | None = None

    @property
    def height(self):
        return self.box[3] - self.box[1]


@dataclass(frozen=True)
class Line:
    words: tuple[Word, ...]
    box: Box

    @classmethod
    def from_words(cls, words):
        words = tuple(words)
        return cls(words, extent(word.box for word in words))


@dataclass(frozen=True)
class Cell:
    row: tuple[int, int]
    column: tuple[int, int]
    box: Box


@dataclass
class VerifiedTable:
    cells: list[tuple[Cell, list[Line]]]
    ambiguous: set[int]
    row_groups: tuple[tuple[tuple[int, int], Line], ...] = ()
    ambiguous_rows: tuple[int, ...] = ()


def extent(boxes):
    left, top, right, bottom = zip(*boxes, strict=True)
    return min(left), min(top), max(right), max(bottom)


def area(box):
    left, top, right, bottom = box
    return max(0, right - left) * max(0, bottom - top)


def overlap(left, right):
    return area((max(left[0], right[0]), max(left[1], right[1]),
                 min(left[2], right[2]), min(left[3], right[3])))


def center(box):
    return (box[0] + box[2]) / 2, (box[1] + box[3]) / 2


def command(args, data=None, *, optional=False, timeout=20):
    env = None
    if args[0] == "tesseract":
        # Independent workers can share the same host CPUs. Limit only Tesseract.
        env = os.environ | {"OMP_THREAD_LIMIT": "1", "OMP_NUM_THREADS": "1"}
    with tempfile.TemporaryFile() as out:
        try:
            result = subprocess.run(args, input=data, stdout=out, stderr=subprocess.DEVNULL,
                                    check=False, timeout=timeout, env=env)
        except subprocess.TimeoutExpired:
            raise ExtractionError("A PDF or OCR tool exceeded its time limit.") from None
        if result.returncode:
            if optional and result.returncode > 0:
                return b""
            raise ExtractionError("A PDF or OCR tool failed.")
        out.seek(0)
        value = out.read(24 * MIB + 1)
        if len(value) > 24 * MIB:
            raise ExtractionError("A PDF or OCR tool exceeded its output bound.")
        return value


def checked_word(text, box, width, height):
    text = " ".join(text.split())
    if not text or any(ord(ch) < 32 or 127 <= ord(ch) < 160 for ch in text):
        return None
    if not all(map(math.isfinite, box)):
        raise ExtractionError("A word has invalid geometry.")
    left, top, right, bottom = box
    clipped = (max(0, left), max(0, top), min(width, right), min(height, bottom))
    return Word(text, clipped) if area(clipped) else None


def native_pages(path, geometry):
    count = len(geometry)
    # Poppler's cropped text pass can drop words from rotated pages. Request
    # media-space text and apply the displayed CropBox ourselves.
    xml = command(["pdftotext", "-f", "1", "-l", str(count), "-bbox-layout", path, "-"])
    document = ET.fromstring(xml)
    pages = document.findall(".//{*}page")
    if len(pages) != count:
        raise ExtractionError("The text extractor returned a different page count.")
    output = []
    for page, (_, rotation, fractions) in zip(pages, geometry, strict=True):
        media_width, media_height = (float(page.attrib[key]) for key in ("width", "height"))
        if not all(math.isfinite(n) and n > 0 for n in (media_width, media_height)):
            raise ExtractionError("A page has invalid text geometry.")
        if rotation in (90, 270):
            # XML page sizes are unrotated even though word boxes are displayed.
            media_width, media_height = media_height, media_width
        left, top, right, bottom = (value * (media_width, media_height)[i % 2]
                                    for i, value in enumerate(fractions))
        width, height = right - left, bottom - top
        lines = []
        for line in page.findall(".//{*}line"):
            words = []
            for node in line.findall("{*}word"):
                box = tuple(float(node.attrib[key]) - (left, top)[i % 2]
                            for i, key in enumerate(("xMin", "yMin", "xMax", "yMax")))
                word = checked_word("".join(node.itertext()), box, width, height)
                if word:
                    words.append(word)
            if words:
                lines.append(Line.from_words(words))
        if sum(len(line.words) for line in lines) > MAX_WORDS:
            raise ExtractionError("A page has too many words.")
        output.append((width, height, lines))
    return output


def image_coverage(path, count):
    report = command(["pdfimages", "-f", "1", "-l", str(count), "-list", path]).decode("ascii")
    areas = defaultdict(float)
    for row in report.splitlines()[2:]:
        fields = row.split()
        if len(fields) >= 16 and fields[2] == "image":
            page = int(fields[0])
            # Poppler reports embedded image sizes and their effective displayed PPI.
            width = 72 * int(fields[3]) / max(1, int(fields[12]))
            height = 72 * int(fields[4]) / max(1, int(fields[13]))
            if min(width, height) >= 24:
                areas[page] += width * height
    return areas


def page_metadata(reader):
    result = []
    if reader.is_encrypted or not 1 <= len(reader.pages) <= 12:
        raise ExtractionError("OCR requires an unencrypted PDF of at most 12 pages.")
    for page in reader.pages:
        # Clip the requested CropBox to the physical medium, as Poppler does.
        crop, media = page.cropbox, page.mediabox
        coordinates = (max(crop.left, media.left), max(crop.bottom, media.bottom),
                       min(crop.right, media.right), min(crop.top, media.top))
        unit = Decimal(str(page.user_unit)) * 1000
        crop_box = [round(Decimal(str(coordinate)) * unit) for coordinate in coordinates]
        media_box = [round(Decimal(str(coordinate)) * unit) for coordinate in media]
        left, bottom, right, top = crop_box
        rotation = page.rotation % 360
        if (rotation not in (0, 90, 180, 270)
                or not all(-(2**31) <= n < 2**31 for n in crop_box)
                or not (0 < right - left <= 50_000_000 and 0 < top - bottom <= 50_000_000)):
            raise ExtractionError("A page has unsupported geometry.")
        result.append((crop_box, rotation, crop_fractions(crop_box, media_box, rotation)))
    return result


def crop_fractions(crop, media, rotation):
    left, bottom, right, top = crop
    ml, mb, mr, mt = media
    width, height = mr - ml, mt - mb
    # Convert the PDF's lower-left crop rectangle to displayed top-left media
    # coordinates using the same right-angle rotation as the page.
    box = (left - ml, mt - top, right - ml, mt - bottom)
    display = displayed_box(box, (360 - rotation) % 360, width, height)
    size = (height, width) if rotation in (90, 270) else (width, height)
    return [value / size[i % 2] for i, value in enumerate(display)]


def raster_page(path, page, width, height):
    # Keep normal pages at 200 DPI and cap the area of larger sheets.
    dpi = min(200, math.floor(72 * math.sqrt((MAX_PAGE_PIXELS - 8192) / (width * height))))
    if dpi < 12:
        raise ExtractionError("A page is too large to rasterize.")
    data = command(["pdftoppm", "-f", str(page), "-l", str(page), "-r", str(dpi),
                    "-cropbox", "-singlefile", "-png", path])
    image = Image.open(BytesIO(data))
    if image.width * image.height > MAX_PAGE_PIXELS:
        raise ExtractionError("A page exceeded the raster limit.")
    image.load()
    return image, data


def recognize(image, data):
    orientation = command(["tesseract", "stdin", "stdout", "--psm", "0", "-l", "osd",
                           "-c", "min_characters_to_try=15"], data, optional=True)
    angle = re.search(rb"^Rotate:\s*(90|180|270)\s*$", orientation, re.MULTILINE)
    correction = int(angle.group(1)) if angle else 0
    rotations = {90: Image.Transpose.ROTATE_270, 180: Image.Transpose.ROTATE_180,
                 270: Image.Transpose.ROTATE_90}
    if correction:
        image = image.transpose(rotations[correction])
        encoded = BytesIO()
        image.save(encoded, "PNG")
        data = encoded.getvalue()
    tsv = command(["tesseract", "stdin", "stdout", "--psm", "3", "-l", "eng", "tsv"], data)
    words = []
    for row in csv.DictReader(StringIO(tsv.decode("utf-8")), delimiter="\t"):
        if row["level"] != "5":
            continue
        left, top, width, height = (int(row[key]) for key in ("left", "top", "width", "height"))
        word = checked_word(row["text"], (left, top, left + width, top + height), *image.size)
        if word:
            source_line = (int(row["block_num"]), int(row["par_num"]), int(row["line_num"]))
            words.append(Word(word.text, word.box, source_line))
        if len(words) > MAX_WORDS:
            raise ExtractionError("A page has too many words.")
    return image, correction, words


def physical_rows(words):
    rows = []
    active = []
    baseline = 0
    for word in sorted(words, key=lambda word: (center(word.box)[1], word.box[0])):
        y = center(word.box)[1]
        if active and y - baseline > .48 * max(word.height, active[-1].height):
            rows.append(Line.from_words(sorted(active, key=lambda word: word.box[0])))
            active = []
        if not active:
            baseline = y
        active.append(word)
    if active:
        rows.append(Line.from_words(sorted(active, key=lambda word: word.box[0])))
    return rows


def separated(left, right, vertical):
    top = min(left.box[1], right.box[1])
    bottom = max(left.box[3], right.box[3])
    return any(left.box[2] <= x <= right.box[0] and first <= top and end >= bottom
               for x, first, end in vertical)


def fragments(line, gap_factor=2, *, vertical=()):
    parts = []
    words = [line.words[0]]
    rightmost = words[0]
    minimum_gap = gap_factor * median(word.height for word in line.words)
    for word in line.words[1:]:
        if word.box[0] - rightmost.box[2] > minimum_gap or separated(rightmost, word, vertical):
            parts.append(Line.from_words(words))
            words = []
        words.append(word)
        if len(words) == 1 or word.box[2] > rightmost.box[2]:
            rightmost = word
    parts.append(Line.from_words(words))
    return parts


def prose_lines(rows):
    # Read recurring two-column gutters down each column instead of interleaving lines.
    i = 0
    while i < len(rows):
        row = rows[i]
        if len(row) == 2:
            left, right = row[0].box[2], row[1].box[0]
            j = i + 1
            while j < len(rows) and len(rows[j]) == 2:
                pair = rows[j]
                lo, hi = max(left, pair[0].box[2]), min(right, pair[1].box[0])
                if hi - lo < .7 * (pair[0].box[3] - pair[0].box[1]):
                    break
                left, right = lo, hi
                j += 1
            if j >= i + 2:
                for column in (0, 1):
                    yield from (pair[column] for pair in rows[i:j])
                i = j
                continue
        yield from row
        i += 1


def table_windows(rows):
    """Find recurring physical columns before asking the model to predict a grid."""
    index = 0
    while index + 2 < len(rows):
        first = rows[index]
        anchors = [line.box[0] for line in first]
        if not 2 <= len(anchors) <= 16:
            index += 1
            continue
        tolerance = 1.7 * median(line.box[3] - line.box[1] for line in first)
        last = index
        consistent = 1
        for j in range(index + 1, min(len(rows), index + 64)):
            row = rows[j]
            gap = min(line.box[1] for line in row) - max(line.box[3] for line in rows[j - 1])
            if gap > 4 * tolerance:
                break
            matches = {
                min(range(len(anchors)), key=lambda i: abs(anchors[i] - line.box[0]))
                for line in row if min(abs(anchor - line.box[0]) for anchor in anchors) <= tolerance
            }
            if len(matches) == len(anchors):
                consistent += 1
                last = j
            elif not matches or j - last > 1:
                break
        if consistent >= 3:
            yield index, last + 1
            index = last + 1
        else:
            index += 1


def crop_around(rows, size, padding):
    left, top, right, bottom = extent(line.box for row in rows for line in row)
    return (max(0, math.floor(left - padding)), max(0, math.floor(top - padding)),
            min(size[0], math.ceil(right + padding)), min(size[1], math.ceil(bottom + padding)))


class TableModel:
    def __init__(self, path):
        self.path = path
        self.model = None

    def predict(self, image, crop):
        import numpy as np

        if self.model is None:
            from paddleocr import TableStructureRecognition

            self.model = TableStructureRecognition(model_name="SLANet_plus",
                                                  model_dir=str(self.path), device="cpu",
                                                  cpu_threads=4, enable_mkldnn=False)
        pixels = np.asarray(image.crop(crop).convert("RGB"))[:, :, ::-1].copy()
        result, = self.model.predict(input=pixels, batch_size=1)
        return grid(result.json["res"], crop)


def grid(result, crop):
    markup = "".join(result["structure"])
    boxes = iter(result["bbox"])
    occupied = set()
    cells = []
    for y, row in enumerate(re.findall(r"<tr>(.*?)</tr>", markup)):
        x = 0
        for attrs in re.findall(r"<td([^>]*)>", row):
            while (y, x) in occupied:
                x += 1
            rs = re.search(r'rowspan="(\d+)"', attrs)
            cs = re.search(r'colspan="(\d+)"', attrs)
            y2, x2 = y + (int(rs.group(1)) if rs else 1), x + (int(cs.group(1)) if cs else 1)
            box = next(boxes, ())
            if len(box) != 8 or not (y < y2 <= 64 and x < x2 <= 16):
                return []
            if not all(math.isfinite(value) for value in box):
                return []
            left, top, right, bottom = (min(box[::2]) + crop[0], min(box[1::2]) + crop[1],
                                        max(box[::2]) + crop[0], max(box[1::2]) + crop[1])
            slots = {(j, i) for j in range(y, y2) for i in range(x, x2)}
            if occupied & slots or not area((left, top, right, bottom)):
                return []
            occupied.update(slots)
            cells.append(Cell((y, y2), (x, x2), (left, top, right, bottom)))
            x = x2
    if next(boxes, None) is not None or not cells:
        return []
    rows = max(cell.row[1] for cell in cells)
    columns = max(cell.column[1] for cell in cells)
    return cells if len(occupied) == rows * columns else []


def assign(words, cells):
    assigned = [[] for _ in cells]
    for word in words:
        fractions = [(overlap(word.box, cell.box) / area(word.box), i)
                     for i, cell in enumerate(cells)]
        best, index = max(fractions)
        if best < .48 or sum(value >= best - .15 for value, _ in fractions) != 1:
            return None
        assigned[index].append(word)
    return assigned


def horizontal_rules(image, left, right, top, bottom, minimum):
    import numpy as np

    region = image.crop((left, top, right, bottom)).convert("L")
    runs = set()
    for row in np.asarray(region) < 160:
        borders = np.flatnonzero(np.diff(row, prepend=False, append=False))
        runs.update((left + a, left + b) for a, b in zip(borders[::2], borders[1::2], strict=True)
                    if b - a >= minimum)
    return sorted(runs)


def upper_groups(model, image, upper, base, crop, leaves, height):
    words = [word for line in upper for word in line.words]
    columns = len(leaves)
    if not words:
        return [], set()
    starts = [line.box[0] for line in leaves]
    centers = [center(line.box)[0] for line in leaves]
    accepted = []
    ambiguous = set()
    aligned_starts = []
    for line in upper:
        near = [i for i, start in enumerate(starts) if abs(start - line.box[0]) <= 1.25 * height]
        aligned_starts.append(near[0] if len(near) == 1 else None)
    expanded = crop_around([upper, *base], image.size, .4 * height)
    grid_cells = model.predict(image, expanded)
    predictions = [cell for cell in grid_cells if cell.row == (0, 1)]
    top = math.ceil(max(line.box[3] for line in upper))
    bottom = math.floor(min(line.box[1] for line in leaves))
    rules = horizontal_rules(image, crop[0], crop[2], top, bottom, 2 * height) if top < bottom else []
    for i, line in enumerate(upper):
        proposed = {cell.column for cell in predictions if cell.column[1] <= columns
                    and all(overlap(word.box, cell.box) >= .7 * area(word.box)
                            for word in line.words)}
        visible = set()
        line_center = center(line.box)[0]
        for start, end in rules:
            if start > line.box[0] + .3 * height or end < line.box[2] - .3 * height:
                continue
            included = [j for j, x in enumerate(centers) if start < x < end]
            # A full-width rule may separate rows but cannot assign separate labels.
            if (included and start < line_center < end
                    and sum(start < center(item.box)[0] < end for item in upper) == 1):
                visible.add((included[0], included[-1] + 1))
        a = aligned_starts[i]
        b = aligned_starts[i + 1] if i + 1 < len(upper) else None
        if not visible and a is not None and b is not None and a < b:
            visible.add((a, b))
        if not visible and a is None:
            # A centered label can identify a span only if no other span has
            # the same visual midpoint and the label stays within its outside edges.
            candidates = [(left, right + 1) for left in range(columns)
                          for right in range(left, columns)
                          if abs(line_center - (centers[left] + centers[right]) / 2) <= .4 * height]
            if len(candidates) == 1:
                left, right = candidates[0]
                if (right - left > 1 and leaves[left].box[0] <= line.box[0]
                        and line.box[2] <= leaves[right - 1].box[2]):
                    visible.add((left, right))
        if len(visible) == 1 and visible == proposed:
            accepted.append((next(iter(visible)), line))
        elif not visible and len(upper) > 1 and a is not None and b is None and len(proposed) == 1:
            first, _ = next(iter(proposed))
            if first == a:
                # A left-aligned start supports its first leaf. Without an
                # end cue, extending it to the final column is speculative.
                accepted.append(((a, a + 1), line))
                ambiguous.update(range(a + 1, columns))
                continue
            ambiguous.update(range(a, columns))
        elif a is not None and (len(upper) > 1 or visible):
            ambiguous.update(range(a, b if b is not None and b > a else columns))
        elif len(upper) > 1 or visible:
            # Preserve leaf evidence when a plausible upper label has no unique anchor.
            ambiguous.update(j for j, x in enumerate(centers) if line.box[0] - height <= x)
    used = set()
    for (a, b), _ in accepted:
        if used.intersection(range(a, b)):
            return [], ambiguous | used | set(range(a, b))
        used.update(range(a, b))
    grounded = []
    for span, line in accepted:
        if ambiguous.intersection(range(*span)):
            ambiguous.update(range(*span))
        else:
            grounded.append((span, line))
    return grounded, ambiguous


def extract_table(model, image, rows, start, end, cursor):
    base = rows[start:end]
    leaf_count = len(base[0])
    height = median(word.height for line in base[0] for word in line.words)
    crop = crop_around(base, image.size, .48 * height)
    if leaf_count == 2:
        left, right = (center(line.box)[0] for line in base[0])
        top = math.ceil(max(line.box[3] for line in base[0]))
        bottom = math.floor(min(line.box[1] for line in base[1]))
        if top >= bottom or not any(
            l < left < right < r for l, r in horizontal_rules(
                image, crop[0], crop[2], top, bottom, right - left
            )
        ):
            return None
    cells = model.predict(image, crop)
    headers = [cell for cell in cells if cell.row[0] == 0]
    if (len(headers) != leaf_count or max(cell.column[1] for cell in cells) != leaf_count
            or any(cell.row != (0, 1) or cell.column != (i, i + 1)
                   for i, cell in enumerate(headers))):
        return None
    words = [word for row in base for line in row for word in line.words]
    assigned = assign(words, cells)
    if assigned is None or any(not assigned[i] for i in range(leaf_count)):
        return None
    # The expected leaf labels must be the model's first row, not displaced by a caption.
    if any(set(assigned[i]) != set(base[0][i].words) for i in range(leaf_count)):
        return None
    body_keys = [tuple(word.text for word in items)
                 for cell, items in zip(cells, assigned, strict=True)
                 if cell.column == (0, 1) and cell.row[0] > 0 and cell.row[1] == cell.row[0] + 1]
    if len(body_keys) < 2 or any(not key for key in body_keys) or len(set(body_keys)) != len(body_keys):
        return None
    parent_row = rows[start - 1] if start > cursor else []
    parents = []
    ambiguous = set()
    if parent_row and max(line.box[3] for line in parent_row) < crop[1]:
        gap = min(line.box[1] for line in base[0]) - max(line.box[3] for line in parent_row)
        parts = [part for line in parent_row for part in fragments(line, 1.35)]
        nearby = [line for line in parts if crop[0] <= center(line.box)[0] <= crop[2]]
        if 0 < gap <= 3 * height and nearby and len(nearby) < leaf_count:
            parents, ambiguous = upper_groups(model, image, nearby, base, crop, base[0], height)
    return cells, assigned, parents, ambiguous


def geometry(mode, data):
    output = command(["rust-rag-ocr-geometry", mode], data)
    return (json.loads(line) for line in output.splitlines())


def aligned_sentence_columns(cells, columns, leaf_row):
    endings = defaultdict(set)
    for cell, lines in cells:
        row, bottom = cell.row
        left, right = cell.column
        if row <= leaf_row or bottom != row + 1:
            continue
        sentence_columns = endings[row]
        if (right == left + 1 and sum(len(line.words) for line in lines) >= 4
                and lines[-1].words[-1].text.rstrip("'\"’”)]").endswith((".", "!", "?"))):
            sentence_columns.add(left)
    aligned = sum(len(row) == columns for row in endings.values())
    return aligned >= 2 and 2 * aligned >= len(endings)


def record_like_header(cells, leaf_row):
    """Withhold a header made of recurring record values."""
    leaves = []
    values = defaultdict(set)
    for cell, lines in cells:
        row, bottom = cell.row
        left, right = cell.column
        if left == 0 or right != left + 1 or bottom != row + 1:
            continue
        text = " ".join(word.text for line in lines for word in line.words)
        kinds = []
        if re.fullmatch(r"[$€£¥][+-]?[0-9][0-9.,]*", text):
            kinds.append(("price",))
        elif re.fullmatch(r"(?:[A-Za-z]+-?[0-9]{2,}|[0-9]{2,}-?[A-Za-z]+)", text):
            kinds.append(("part_code",))
        code = re.fullmatch(r"([A-Za-z]{2,12})-(?=[A-Z0-9]*[0-9])[A-Z0-9]{2,12}", text)
        if code:
            kinds.append(("code_family", code[1]))
        if "/" in text and any(a.box[3] > b.box[1] and b.box[3] > a.box[1]
                               for a, b in zip(lines, lines[1:])):
            measurements = list(re.finditer(
                r"[+-]?[0-9]+(?:[.,][0-9]+)?\s*([A-Za-zµ°]{1,8}(?:/[A-Za-zµ°]{1,8})?)", text))
            units = [match[1] for match in measurements]
            if (len(units) >= 2 and any("/" in unit for unit in units)
                    and " ".join(match[0] for match in measurements) == text):
                kinds.append(("compound_measurement", *units))
        for kind in kinds:
            if row == leaf_row:
                leaves.append((left, kind, text))
            elif row > leaf_row:
                values[left, kind].add(text)
    matched = defaultdict(set)
    for left, kind, text in leaves:
        if len(values[left, kind] - {text}) >= 2:
            matched[kind[0]].add(left)
    if len(matched["price"] | matched["part_code"]) >= 2:
        return True
    codes, measurements = matched["code_family"], matched["compound_measurement"]
    return bool(codes and measurements and len(codes | measurements) >= 2)


def verify_cells(cells, parent_frames, columns, leaf_row, *, ruled=False, row_groups=()):
    """Ground the grid in observed words. A rejected cell stays searchable as prose."""
    observed = []
    for cell, lines in cells:
        crosses_leaf = cell.row[0] < leaf_row < cell.row[1]
        crosses_body = cell.row[0] <= leaf_row < cell.row[1] - 1
        if not (crosses_leaf or crosses_body) and 0 < len(lines) <= 32 and all(
            overlap(word.box, cell.box) >= .8 * area(word.box)
            for line in lines for word in line.words
        ):
            observed.append((cell, lines))
    if not ruled and (aligned_sentence_columns(observed, columns, leaf_row)
                      or record_like_header(observed, leaf_row)):
        return None
    leaves = [cell.column[0] for cell, _ in observed
              if cell.row == (leaf_row, leaf_row + 1) and cell.column[1] == cell.column[0] + 1]
    keys = [(cell.row[0], " ".join(word.text for line in lines for word in line.words).lower())
            for cell, lines in observed if cell.row[0] > leaf_row and cell.column == (0, 1)]
    if len(leaves) < 2 or 0 not in leaves or len(keys) < 2:
        return None
    counts = Counter(key for _, key in keys)
    scopes = {row: " ".join(word.text for word in label.words).lower()
              for span, label in row_groups for row in range(*span)}
    scoped = Counter((key, scopes.get(row)) for row, key in keys)
    invalid_rows = {row for row, key in keys if counts[key] > 1
                    and (row not in scopes or scoped[key, scopes[row]] > 1)}
    if invalid_rows:
        observed = [(cell, lines) for cell, lines in observed
                    if not (cell.column == (0, 1) and cell.row[0] in invalid_rows)]
        if len(invalid_rows) == len(keys):
            return None
    observed.extend(parent_frames)
    return VerifiedTable(observed, set(range(columns)) - set(leaves), tuple(row_groups))


def rule_layout(image):
    pixels = BytesIO()
    image.convert("L").save(pixels, format="PPM")
    return next(geometry("rules", pixels.getvalue()))


def framed_rows(cells, assigned):
    title, stubs, lines = (physical_rows(words) for words in assigned)
    if len(title) != 1 or not 4 <= len(stubs) <= 64 or not lines:
        return None
    height = median(line.box[3] - line.box[1] for line in (*stubs, *lines))
    tolerance = .65 * height
    baselines = [center(line.box)[1] for line in stubs]
    gaps = [b - a for a, b in zip(baselines, baselines[1:])]
    cadence = median(gaps)
    starts = [stub.box[0] for stub in stubs]
    if (not all(max(1.8 * height, .72 * cadence) <= gap <= 1.35 * cadence for gap in gaps)
            or max(starts) - min(starts) > height or any(len(stub.words) > 8 for stub in stubs)):
        return None

    values = [[] for _ in stubs]
    for line in lines:
        y = center(line.box)[1]
        row = bisect_right(baselines, y + tolerance) - 1
        if row < 0:
            return None
        previous = values[row]
        if not previous:
            if abs(y - baselines[row]) > tolerance:
                return None
        elif (row == 0 or len(previous) == 3 or line.box[1] - previous[-1].box[3] > height
                or y - center(previous[-1].box)[1] > .70 * cadence
                or abs(line.box[0] - previous[0].box[0]) > height):
            return None
        previous.append(line)
    paired = sum(bool(value) for value in values[1:])
    if len(values[0]) != 1 or paired < 3 or paired * 3 < 2 * (len(stubs) - 1):
        return None

    bounds = [cells[1].box[1], *(y - tolerance for y in baselines[1:]), cells[1].box[3]]
    proposed = []
    for row, (stub, value) in enumerate(zip(stubs, values, strict=True)):
        for column, items in enumerate(([stub], value)):
            left, _, right, _ = cells[column + 1].box
            box = (left, bounds[row], right, bounds[row + 1])
            proposed.append((Cell((row, row + 1), (column, column + 1), box), items))
    return verify_cells(proposed, [], 2, 0)


def ruled_tables(proposals, words):
    for proposal in proposals:
        x, y = proposal["x"], proposal["y"]
        columns = len(x) - 1
        cells = [Cell((top, bottom), (left, right), (x[left], y[top], x[right], y[bottom]))
                 for top, bottom, left, right in proposal["cells"]]
        slots = {(row, col): i for i, cell in enumerate(cells)
                 for row in range(*cell.row) for col in range(*cell.column)}
        assigned = [[] for _ in cells]
        for word in words:
            cx, cy = center(word.box)
            index = slots.get((bisect_right(y, cy) - 1, bisect_right(x, cx) - 1))
            if index is not None:
                assigned[index].append(word)
        if len(cells) == 3:
            table = framed_rows(cells, assigned)
            if table is not None:
                yield table, 0
            continue
        # A spanning top band is a parent only when the next band visibly
        # contains the leaf row. Deeper structures retain their literal text.
        leaf_row = int(any(cell.row == (0, 1) and cell.column[1] - cell.column[0] > 1
                           and items for cell, items in zip(cells, assigned, strict=True)))
        parents = []
        if leaf_row == 0:
            frames = [Cell((0, 1), (left, right), (x[left], top, x[right], bottom))
                      for top, bottom, left, right in proposal["frames"]]
            candidates = [(frame, physical_rows([word for word in words
                           if overlap(word.box, frame.box) >= .8 * area(word.box)]))
                          for frame in frames]
            candidates = [(frame, lines) for frame, lines in candidates if 0 < len(lines) <= 32]
            parents = [(frame, lines) for frame, lines in candidates if sum(
                overlap(frame.box, other.box) > 0 for other, _ in candidates
            ) == 1]
        shift = int(bool(parents))
        cells = [Cell(tuple(i + shift for i in cell.row), cell.column, cell.box) for cell in cells]
        table = verify_cells(((cell, physical_rows(items))
                              for cell, items in zip(cells, assigned, strict=True)),
                             parents, columns, leaf_row + shift, ruled=True)
        if table is not None:
            yield table, leaf_row + shift


def words_in_band(words, bounds, top, bottom):
    return [word for word in words if word.box[0] < bounds[-1] and word.box[2] > bounds[0]
            and word.box[1] < bottom and word.box[3] > top]


def column_slots(line, bounds, top, bottom):
    slots = [[] for _ in bounds[1:]]
    for word in line.words:
        left, first, right, end = word.box
        column = bisect_right(bounds, left) - 1
        if not 0 <= column < len(slots) or right > bounds[column + 1] or first < top or end > bottom:
            return None
        slots[column].append(word)
    return slots


def partial_body(words, bounds, top, bottom, height, dividers):
    selected = words_in_band(words, bounds, top, bottom)
    rows = []
    baseline = None
    votes = [0] * len(dividers)
    label = None
    for line in physical_rows(selected):
        slots = column_slots(line, bounds, top, bottom)
        if slots is None:
            return None
        occupied = [i for i, slot in enumerate(slots) if slot]
        if not rows and label is None and occupied == [0] and line.box[1] - top <= 2 * height:
            label = line
        elif occupied[0] == 0 and len(occupied) >= 3:
            if baseline is not None and line.box[1] - baseline <= 1.25 * height:
                return None
            baseline = line.box[1]
            rows.append([[Line.from_words(slot)] if slot else [] for slot in slots])
            for index, rails in enumerate(dividers):
                if any(separated(a, b, rails) for a, b in zip(line.words, line.words[1:])):
                    votes[index] += 1
        elif len(occupied) == 1 and rows and line.box[1] - baseline <= 2 * height:
            column = occupied[0]
            previous = rows[-1][column]
            tolerance = .45 * height if column == 0 else height
            if len(previous) != 1 or abs(previous[0].box[0] - line.box[0]) > tolerance:
                return None
            previous.append(line)
        else:
            return None
    if len(rows) < 2 or min(votes) < 2:
        return None
    if label is not None and min(row[0][0].box[0] for row in rows) - label.box[0] < .5 * height:
        label = None
    return rows, label


def same_span(left, right):
    return max(abs(left[1] - right[1]), abs(left[2] - right[2])) <= RAIL_TOLERANCE


def band_table(rows, bounds, top, bottom, groups):
    cells = ((Cell((i, i + 1), (j, j + 1), (bounds[j], top, bounds[j + 1], bottom)), items)
             for i, row in enumerate(rows) for j, items in enumerate(row) if items)
    return verify_cells(cells, [], len(bounds) - 1, 0, row_groups=groups)


def partial_tables(layout, words):
    horizontal = sorted(layout["horizontal"])
    vertical = layout["vertical"]
    if not vertical or len(horizontal) < 3:
        return
    lines = physical_rows(words)
    attempts = 0
    for line in lines:
        left, top, right, bottom = line.box
        height = median(word.height for word in line.words)
        leaves = fragments(line, vertical=vertical)
        columns = len(leaves)
        if not 3 <= columns <= 16:
            continue
        surrounding = [rail for rail in horizontal if rail[1] <= left and right <= rail[2]]
        cap = next((rail for rail in reversed(surrounding) if top - 2 * height <= rail[0] < top), None)
        floor = next((rail for rail in surrounding if bottom < rail[0] <= bottom + 2 * height), None)
        if cap is None or floor is None or not same_span(cap, floor):
            continue
        matching = [rail for rail in horizontal if same_span(cap, rail)]
        # The header must be the first matched band. A later body band cannot seed a table.
        if any(RAIL_TOLERANCE < cap[0] - rail[0] <= 20 * height for rail in matching):
            continue
        if any(other is not line and cap[0] < other.box[3] and other.box[1] < floor[0]
               and other.box[0] < cap[2] and cap[1] < other.box[2] for other in lines):
            continue

        cuts = []
        dividers = []
        for a, b in zip(leaves, leaves[1:]):
            first_y = min(a.box[1], b.box[1], cap[0] + RAIL_TOLERANCE)
            last_y = max(a.box[3], b.box[3], floor[0] - RAIL_TOLERANCE)
            edge = [x for x, first, end in vertical if a.box[2] < x < b.box[0]
                    and first <= first_y and end >= last_y]
            if edge and max(edge) - min(edge) <= RAIL_TOLERANCE:
                x = median(edge)
                cuts.append(x)
                dividers.append([rail for rail in vertical if abs(rail[0] - x) <= RAIL_TOLERANCE])
            elif not edge and b.box[0] - a.box[2] > 2 * height:
                cuts.append((a.box[2] + b.box[0]) / 2)
            else:
                break
        if len(cuts) != columns - 1 or not dividers:
            continue
        attempts += 1
        if attempts > 16:
            return
        bounds = [cap[1], *cuts, cap[2]]
        rows = [[[leaf] for leaf in leaves]]
        groups = []
        start = floor[0]
        for end, _, _ in matching:
            if end - start <= RAIL_TOLERANCE:
                continue
            if end - start > 20 * height:
                break
            band = partial_body(words, bounds, start, end, height, dividers)
            if band is None or len(rows) + len(band[0]) > 64:
                rows = []
                break
            body, label = band
            if label is not None:
                groups.append(((len(rows), len(rows) + len(body)), label))
            rows.extend(body)
            start = end
        if len(rows) < 3:
            continue
        table = band_table(rows, bounds, cap[0], start, groups)
        if table is not None:
            yield table, 0


def aligned_band(words, bounds, starts, top, bottom, height, *, labelled):
    lines = physical_rows(words_in_band(words, bounds, top, bottom))
    label = None
    if labelled and lines:
        first = lines[0]
        if (top <= first.box[1] <= top + 4 * height and first.box[3] <= bottom
                and first.box[2] <= bounds[-1] and first.box[2] > bounds[1]
                and abs(first.box[0] - starts[0]) <= .6 * height and len(fragments(first)) == 1):
            label, lines = first, lines[1:]
    if len(lines) < 2 or any(b.box[1] - a.box[1] <= 1.25 * height for a, b in zip(lines, lines[1:])):
        return None
    rows = []
    for line in lines:
        slots = column_slots(line, bounds, top, bottom)
        if slots is None or any(not words or abs(words[0].box[0] - start) > .6 * height
                                for words, start in zip(slots, starts, strict=True)):
            return None
        rows.append([[Line.from_words(words)] for words in slots])
    return rows, label


def underlined_tables(layout, words):
    """Ground horizontal-only groups using an underline and a repeated full grid."""
    horizontal = sorted(layout["horizontal"])
    if layout["vertical"] or len(horizontal) < 2:
        return
    attempts = 0
    for line in physical_rows(words):
        left, top, right, bottom = line.box
        height = median(word.height for word in line.words)
        leaves = fragments(line)
        if not 3 <= len(leaves) <= 16:
            continue
        underline = next((rail for rail in horizontal if bottom < rail[0] <= bottom + 2 * height
                          and rail[1] <= left and right <= rail[2]), None)
        if underline is None:
            continue
        floor, x0, x1 = underline
        if any(same_span(underline, rail) and RAIL_TOLERANCE < floor - rail[0] <= 20 * height
               for rail in horizontal):
            continue
        starts = [leaf.box[0] for leaf in leaves]
        bounds = [x0, *((a.box[2] + b.box[0]) / 2 for a, b in zip(leaves, leaves[1:])), x1]
        # Each band needs a complete grid. A short separator can terminate
        # the preceding label's scope, but cannot establish a following one.
        rails = [rail for rail in horizontal if floor + RAIL_TOLERANCE < rail[0] <= floor + 40 * height
                 and abs(rail[1] - x0) <= RAIL_TOLERANCE
                 and bounds[1] < rail[2] <= x1 + RAIL_TOLERANCE]
        if len(rails) == 1:
            closing = [rail for rail in layout["faint_closures"]
                       if rails[0][0] + RAIL_TOLERANCE < rail[0] <= floor + 40 * height
                       and same_span(underline, rail)]
            if len(closing) == 1:
                rails.extend(closing)
        if len(rails) != 2 or not same_span(underline, rails[-1]):
            continue
        attempts += 1
        if attempts > 16:
            return
        separator = rails[0]
        first = aligned_band(words, bounds, starts, floor, separator[0], height, labelled=True)
        complete = same_span(underline, separator)
        second = aligned_band(words, bounds, starts, separator[0], rails[-1][0], height,
                              labelled=complete)
        if first is None or second is None or first[1] is None or (complete and second[1] is None):
            continue
        rows = [[[leaf] for leaf in leaves]]
        groups = []
        for body, label in (first, second):
            if label is not None:
                groups.append(((len(rows), len(rows) + len(body)), label))
            rows.extend(body)
        if len(rows) > 64:
            continue
        table = band_table(rows, bounds, top, rails[-1][0], groups)
        if table is not None:
            yield table, 0


def lane_parents(image, upper, leaves, bounds):
    """Anchor an upper label to a visible partial rule or a unique alignment."""
    if not upper:
        return [], set()
    height = median(word.height for leaf in leaves for word in leaf.words)
    top = math.ceil(max(line.box[3] for line in upper))
    bottom = math.floor(min(line.box[1] for line in leaves))
    if not 0 < bottom - top <= 3 * height:
        return [], set()
    centers = [center(line.box)[0] for line in leaves]
    starts = []
    for line in upper:
        matches = [i for i, leaf in enumerate(leaves) if i > 0
                   if abs(leaf.box[0] - line.box[0]) <= height]
        starts.append(matches[0] if len(matches) == 1 else None)
    rules = horizontal_rules(image, bounds[0], bounds[-1], top, bottom, 2 * height)
    centered = []
    for line in upper:
        cx, cy = center(line.box)
        candidates = []
        for left in range(1, len(leaves)):
            for right in range(left + 1, len(leaves)):
                offset = cx - (centers[left] + centers[right]) / 2
                if (abs(offset) <= 1.4 * height and bounds[left] <= line.box[0]
                        and line.box[2] <= bounds[right + 1]):
                    candidates.append((left, right + 1, offset, cy))
        centered.append(candidates)
    parents = []
    ambiguous = set()
    for i, line in enumerate(upper):
        spans = set()
        for start, end in rules:
            included = [j for j, x in enumerate(centers) if start < x < end]
            if (included and 0 not in included and start <= line.box[0] + .3 * height
                    and line.box[2] - .3 * height <= end
                    and sum(start < center(other.box)[0] < end for other in upper) == 1):
                spans.add((included[0], included[-1] + 1))
        ruled = bool(spans)
        a = starts[i]
        b = starts[i + 1] if i + 1 < len(starts) else None
        aligned = a is not None and any(
            other is not None and abs(center(other_line.box)[1] - center(line.box)[1]) <= height / 2
            for j, (other, other_line) in enumerate(zip(starts, upper, strict=True)) if j != i
        )
        if not spans and aligned:
            if b is not None and a < b:
                spans.add((a, b))
            elif i == len(starts) - 1:
                spans.add((a, a + 1))
                ambiguous.update(range(a + 1, len(leaves)))
        if not spans and a is None:
            # Two neighboring labels can corroborate a consistent visual
            # offset from the leaf midpoints. A lone label cannot do so.
            for left, right, offset, cy in centered[i]:
                if any((right == l or r == left) and abs(offset - other) <= .35 * height
                       and abs(cy - y) <= height / 2
                       for j, candidates in enumerate(centered) if j != i
                       for l, r, other, y in candidates):
                    spans.add((left, right))
        if len(spans) == 1:
            left, right = next(iter(spans))
            if (ruled or aligned or (bounds[left] - .3 * height <= line.box[0]
                                     and line.box[2] <= bounds[right] + .3 * height)):
                parents.append((Cell((0, 1), (left, right), line.box), [line]))
                continue
        if spans:
            ambiguous.update(column for left, right in spans for column in range(left, right))
        elif a is None and not ruled and centered[i]:
            ambiguous.update(column for left, right, offset, _ in centered[i]
                             if abs(offset) <= .35 * height for column in range(left, right))
        elif len(upper) > 1:
            first = a if a is not None else max(0, bisect_right(bounds, line.box[0]) - 1)
            last = b if b is not None and b > first else min(len(leaves), first + 1)
            ambiguous.update(range(first, last))
    counts = defaultdict(int)
    for cell, _ in parents:
        for column in range(*cell.column):
            counts[column] += 1
    ambiguous.update(column for column, count in counts.items() if count > 1)
    conflicts = [cell for cell, _ in parents if ambiguous.intersection(range(*cell.column))]
    ambiguous.update(column for cell in conflicts for column in range(*cell.column))
    return [(cell, lines) for cell, lines in parents if cell not in conflicts], ambiguous


def lane_tables(image, words, horizontal=()):
    coordinates = "".join("\t".join(str(value) for value in word.box) + "\n" for word in words)
    if horizontal:
        rails = "".join("\t".join(str(value) for value in rail) + "\n" for rail in horizontal)
        coordinates = rails + "\n" + coordinates
    for proposal in geometry("lanes", coordinates.encode("ascii")):
        def lines(groups):
            return [Line.from_words(words[i] for i in group) for group in groups]

        cuts = proposal["cuts"]
        bounds = [0, *cuts, image.width]
        cells = [(tuple(cell["row"]), tuple(cell["column"]), lines(cell["lines"]))
                 for cell in proposal["cells"]]
        leaves = [Line.from_words(word for line in items for word in line.words)
                  for row, _, items in cells if row == (0, 1)]
        if len(leaves) != len(bounds) - 1:
            continue
        parent_bounds = [max(0, math.floor(leaves[0].box[0] - 2 * leaves[0].words[0].height)),
                         *cuts, min(image.width, math.ceil(leaves[-1].box[2]
                                                         + 2 * leaves[-1].words[0].height))]
        upper = [line for line in lines(proposal["upper"])
                 if parent_bounds[0] <= center(line.box)[0] <= parent_bounds[-1]]
        parents, ambiguous = lane_parents(image, upper, leaves, parent_bounds)
        shift = int(bool(parents))
        table = verify_cells(((Cell(tuple(i + shift for i in row), column,
                                    (bounds[column[0]], 0, bounds[column[1]], image.height)), items)
                              for row, column, items in cells), parents, len(leaves), shift)
        if table is not None:
            table.ambiguous.update(ambiguous)
            table.ambiguous_rows = tuple(row + shift for row in proposal["ambiguous_rows"])
            yield table, shift


def model_tables(model, image, words, vertical):
    rows = [fragments(line, vertical=vertical) for line in physical_rows(words)]
    cursor = 0
    for start, end in table_windows(rows):
        if start < cursor:
            continue
        table = extract_table(model, image, rows, start, end, cursor)
        if table is None:
            continue
        cells, assigned, parents, ambiguous = table
        shift = int(bool(parents))
        parents = [(Cell((0, 1), span, line.box), [line]) for span, line in parents]
        verified = verify_cells(((Cell(tuple(i + shift for i in cell.row), cell.column, cell.box),
                                  physical_rows(items))
                                 for cell, items in zip(cells, assigned, strict=True)),
                                parents, max(cell.column[1] for cell in cells), shift)
        if verified is not None:
            verified.ambiguous.update(ambiguous)
            yield verified, shift
            cursor = end


def ink_box(image):
    return image.point([255 if value < 160 else 0 for value in range(256)]).getbbox()


def optical_fragment(image, expected, mode):
    margin = 10
    padded = Image.new("L", (image.width + 2 * margin, image.height + 2 * margin), 255)
    padded.paste(image, (margin, margin))
    buffer = BytesIO()
    padded.save(buffer, "PNG")
    output = command(["tesseract", "stdin", "stdout", "--dpi", "200", "--psm", str(mode),
                      "-l", "eng", "tsv"], buffer.getvalue(), timeout=.25)
    rows = csv.DictReader(StringIO(output.decode("utf-8")), delimiter="\t")
    words = [row for row in rows if row["level"] == "5" and row["text"].strip()]
    if len(words) != 1 or words[0]["text"] != expected or not float(words[0]["conf"]) >= 85:
        return False
    left, top, width, height = (int(words[0][key]) for key in ("left", "top", "width", "height"))
    left, top = left - margin, top - margin
    right, bottom = left + width, top + height
    ink = ink_box(image)
    if ink is None:
        return False
    l, t, r, b = ink
    return (0 <= left <= l + 1 < right <= image.width and right >= r - 1
            and 0 <= top <= t + 1 < bottom <= image.height and bottom >= b - 1)


def rule_pipe(image, word, vertical):
    left, top, right, bottom = word.box
    if (not word.text.startswith("|") or not 0 < right - left <= 512
            or not 12 <= word.height <= 128):
        return None
    margin = math.floor(.6 * word.height)
    nearby = [x for x, first, end in vertical
              if first <= top - margin and end >= bottom + margin]
    if word.text == "|" and right - left <= .5 * word.height:
        for x in nearby:
            if left <= x < right:
                ink = ink_box(image.crop(word.box).convert("L"))
                if ink is not None and x - 1 <= left + ink[0] < left + ink[2] <= x + 2:
                    return ""
    suffix = word.text[1:]
    if len(suffix) >= 2 and suffix.isalpha() and any(1 <= left - x <= margin for x in nearby):
        return suffix
    return None


def optical_gap(image, word, units):
    # A measurement token is eligible only when the original raster has a
    # dominant blank gap. The reread may insert a space but cannot change glyphs.
    match = re.fullmatch(r"([0-9]{2,})([A-Za-z]{2,})", word.text)
    if match is None or match[2] not in units:
        return None
    left, top, right, bottom = map(int, word.box)
    width, height = right - left, bottom - top
    if not (0 < width <= 512 and 12 <= height <= 128):
        return None
    crop = image.crop((left, top, right, bottom)).convert("L")
    mask = bytes(value < 160 for value in crop.tobytes())
    occupied = [i for i in range(width) if any(mask[i::width])]
    gaps = sorted(((b - a - 1, a + 1, b) for a, b in zip(occupied, occupied[1:])
                   if b > a + 1), reverse=True)
    if len(gaps) < 2:
        return None
    gap, start, end = gaps[0]
    if 2 * gap < height or gap < 2 * gaps[1][0]:
        return None
    return (crop.crop((0, 0, start, height)), crop.crop((end, 0, width, height))), match.groups()


def corroborated_units(cells, leaf):
    peers = defaultdict(lambda: defaultdict(set))
    for cell, lines in cells:
        if (cell.row[0] <= leaf or cell.column == (0, 1) or cell.column[1] != cell.column[0] + 1
                or len(lines) != 1 or len(lines[0].words) != 2):
            continue
        number, unit = (word.text for word in lines[0].words)
        if number.isascii() and number.isdecimal() and unit in MEASUREMENT_UNITS:
            peers[cell.column][unit].add(number)
    return {column: {unit for unit, numbers in units.items() if len(numbers) >= 2}
            for column, units in peers.items()}


def displayed_box(box, correction, width, height):
    left, top, right, bottom = box
    if correction == 90:
        return top, width - right, bottom, width - left
    if correction == 180:
        return width - right, height - bottom, width - left, height - top
    if correction == 270:
        return height - bottom, left, height - top, right
    return box


def restore_skewed_prose(words, remaining):
    rows = physical_rows(remaining)
    source_lines = defaultdict(list)
    for word in words:
        if word.source_line is not None:
            source_lines[word.source_line].append(word)

    def merge(first, second):
        members = sorted(first.words + second.words, key=lambda word: word.box[0])
        source_line = members[0].source_line
        if len(members) < 6 or source_line is None:
            return None
        observed = source_lines[source_line]
        # Require the complete, exclusive OCR line in its original order.
        if len(members) != len(observed) or any(a is not b for a, b in zip(members, observed, strict=True)):
            return None
        height = median(word.height for word in members)
        if any(not 0 <= b.box[0] - a.box[2] <= .9 * height for a, b in zip(members, members[1:])):
            return None
        start_x, start_y = center(members[0].box)
        end_x, end_y = center(members[-1].box)
        if end_x - start_x < 8 * height:
            return None
        slope = (end_y - start_y) / (end_x - start_x)
        if not .008 <= abs(slope) <= .035:
            return None
        residuals = [y - slope * x for x, y in (center(word.box) for word in members)]
        if max(residuals) - min(residuals) > .34 * height:
            return None
        return Line.from_words(members)

    i = 0
    while i < len(rows):
        restored = merge(rows[i], rows[i + 1]) if i + 1 < len(rows) else None
        if restored is None:
            yield rows[i]
            i += 1
        else:
            yield restored
            i += 2


def page_content(words, image, width, height, correction, table_model, native_lines):
    tables = []
    regions = []
    probes = 0
    actual_size = (height, width) if correction in (90, 270) else (width, height)

    def emit(line, units=()):
        nonlocal probes
        box = displayed_box(line.box, correction, width, height)
        coordinates = [max(0, min(SCALE, round(value * SCALE / actual_size[i % 2])))
                       for i, value in enumerate(box)]
        if not area(coordinates):
            raise ExtractionError("A text region is too small for the coordinate scale.")
        text = " ".join(word.text for word in line.words)
        if units and probes < MAX_OPTICAL_PROBES and len(line.words) == 1:
            proposal = optical_gap(image, line.words[0], units)
            if proposal is not None:
                probes += 1
                crops, parts = proposal
                if all(optical_fragment(crop, expected, mode) for mode in (7, 8)
                       for crop, expected in zip(crops, parts, strict=True)):
                    text = " ".join(parts)
        index = len(regions)
        regions.append({"rect": coordinates, "text": text})
        return index

    claimed = set()
    blocks = []
    vertical = ()

    def add(proposals):
        for table, leaf in proposals:
            if vertical and any(len(fragments(line, math.inf, vertical=vertical)) > 1
                                for _, lines in table.cells for line in lines):
                continue
            table_words = [word for _, lines in table.cells for line in lines for word in line.words]
            table_words.extend(word for _, label in table.row_groups for word in label.words)
            identities = {id(word) for word in table_words}
            if len(identities) != len(table_words) or claimed.intersection(identities):
                continue
            claimed.update(identities)
            box = extent(word.box for word in table_words)
            blocks.append((box[1], box[0], table, leaf))

    if image is not None and words:
        layout = rule_layout(image)
        vertical = layout["vertical"]
        if vertical and native_lines is None:
            corrected = []
            for word in words:
                suffix = rule_pipe(image, word, vertical)
                if suffix == "":
                    continue
                if suffix and probes < MAX_OPTICAL_PROBES:
                    probes += 1
                    crop = image.crop(word.box).convert("L")
                    if all(optical_fragment(crop, suffix, mode) for mode in (7, 8)):
                        word = Word(suffix, word.box, word.source_line)
                corrected.append(word)
            words = corrected
        add(ruled_tables(layout["tables"], words))
        add(partial_tables(layout, words))
        add(underlined_tables(layout, words))
        add(lane_tables(image, [word for word in words if id(word) not in claimed],
                        () if vertical else layout["horizontal"]))
        add(model_tables(table_model, image, [word for word in words if id(word) not in claimed], vertical))

    if native_lines is not None and not blocks:
        for line in native_lines:
            emit(line)
        return regions, tables
    remaining = [word for word in words if id(word) not in claimed]
    rows = [fragments(line, vertical=vertical) for line in restore_skewed_prose(words, remaining)]
    cursor = 0
    for top, _, table, leaf in sorted(blocks, key=lambda item: item[:2]):
        end = cursor
        while end < len(rows) and min(line.box[1] for line in rows[end]) < top:
            end += 1
        for line in prose_lines(rows[cursor:end]):
            emit(line)
        cursor = end
        output = []
        pending_groups = {span[0]: (span, label) for span, label in table.row_groups}
        row_groups = []
        units = corroborated_units(table.cells, leaf) if native_lines is None else {}
        for cell, lines in sorted(table.cells, key=lambda item: (item[0].row, item[0].column)):
            group = pending_groups.pop(cell.row[0], None)
            if group is not None:
                span, label = group
                row_groups.append({"row": list(span), "region": emit(label)})
            role = ("column_header" if cell.row[0] <= leaf else
                    "row_header" if cell.column == (0, 1) else "body")
            cell_units = units.get(cell.column, ()) if role == "body" and len(lines) == 1 else ()
            refs = [emit(line, cell_units) for line in lines]
            if role == "row_header" and cell.row[0] in table.ambiguous_rows:
                continue
            output.append({"row": list(cell.row), "column": list(cell.column),
                           "role": role, "regions": refs})
        columns = max(cell.column[1] for cell, _ in table.cells)
        rendition = {"cells": output,
                     "ambiguous_columns": sorted(i for i in table.ambiguous if i < columns)}
        if row_groups:
            rendition["row_groups"] = row_groups
        tables.append(rendition)
    for line in prose_lines(rows[cursor:]):
        emit(line)
    return regions, tables


def extract(path, table_model):
    reader = PdfReader(path, strict=True)
    geometry = page_metadata(reader)
    native = native_pages(path, geometry)
    coverage = image_coverage(path, len(geometry))
    pages = []
    for number, ((crop_box, rotation, _), (width, height, lines)) in enumerate(
        zip(geometry, native, strict=True), 1
    ):
        image = None
        correction = 0
        method = "text_layer"
        native_lines = lines
        words = [word for line in lines for word in line.words]
        # Use one rendition for the full displayed page. A substantial image
        # makes the text layer insufficient even if it contains headings.
        if not words or coverage[number] >= .08 * width * height:
            image, raster = raster_page(path, number, width, height)
            image, correction, words = recognize(image, raster)
            width, height = image.size
            method = "raster_ocr"
            native_lines = None
        elif (sum(line.box[2] - line.box[0] > line.box[3] - line.box[1] for line in lines)
              > len(lines) / 2
              and next(table_windows([fragments(line) for line in physical_rows(words)]), None)):
            image, _ = raster_page(path, number, width, height)
            sx, sy = image.width / width, image.height / height
            native_lines = [Line.from_words(Word(word.text, tuple(value * (sx, sy)[i % 2]
                                       for i, value in enumerate(word.box))) for word in line.words)
                            for line in lines]
            words = [word for line in native_lines for word in line.words]
            width, height = image.size
        regions, tables = page_content(words, image, width, height, correction, table_model,
                                       native_lines)
        pages.append({"number": number, "crop_box_mpt": crop_box, "pdf_rotation_cw": rotation,
                      "method": method, "regions": regions, "tables": tables})
    count = sum(len(page["regions"]) for page in pages)
    characters = sum(len(region["text"]) + 1 for page in pages for region in page["regions"])
    if not 0 < count <= MAX_REGIONS or characters + 2 * len(pages) > MAX_CHARACTERS:
        raise ExtractionError("The PDF has no readable text or exceeds the rendition limit.")
    return {"version": 1, "pages": pages}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--table-model", type=Path, required=True)
    args = parser.parse_args()
    if not all((args.table_model / name).is_file()
               for name in ("inference.json", "inference.pdiparams", "inference.yml")):
        parser.error("Provide the pinned local SLANet_plus model directory.")
    os.environ["LC_ALL"] = "C.UTF-8"
    os.environ["OMP_THREAD_LIMIT"] = "4"
    os.environ["OMP_NUM_THREADS"] = "4"
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK"] = "True"
    os.environ["PADDLE_PDX_LOCAL_FONT_FILE_PATH"] = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
    logging.disable(logging.CRITICAL)
    resource.setrlimit(resource.RLIMIT_FSIZE, (24 * MIB, 24 * MIB))
    raw = sys.stdin.buffer.read(MAX_INPUT + 1)
    try:
        if not 0 < len(raw) <= MAX_INPUT:
            raise ExtractionError("The PDF is empty or exceeds 16 MiB.")
        with tempfile.TemporaryDirectory() as work, redirect_stdout(sys.stderr):
            path = str(Path(work) / "input.pdf")
            Path(path).write_bytes(raw)
            result = extract(path, TableModel(args.table_model))
            data = json.dumps(result, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(data) > MAX_OUTPUT:
            raise ExtractionError("The PDF exceeded the rendition byte limit.")
    except ExtractionError as error:
        print(error, file=sys.stderr)
        return 1
    except Exception:
        # Parser exceptions may contain source text or untrusted PDF metadata.
        print("The PDF OCR extraction failed.", file=sys.stderr)
        return 1
    sys.stdout.buffer.write(data)
    return 0


if __name__ == "__main__":
    sys.exit(main())
