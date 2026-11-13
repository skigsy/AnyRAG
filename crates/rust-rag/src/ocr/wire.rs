//! Validate worker data and materialize literal citations inside the indexer.
use super::Passage;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

pub(super) const MAX_RESPONSE_BYTES: usize = 12 * 1024 * 1024;
const MAX_PAGES: usize = 12;
const MAX_REGIONS: usize = 32_768;
const MAX_CHARACTERS: usize = 2_000_000;
const MAX_GRID_SLOTS: usize = 200_000;
const COORDINATE_SCALE: u32 = 1_000_000;
const PAGE_SEPARATOR: &str = "\n\n";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    pages: Vec<Page>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Page {
    number: u16,
    crop_box_mpt: [i32; 4],
    pdf_rotation_cw: u16,
    method: Method,
    regions: Vec<Region>,
    tables: Vec<Table>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    TextLayer,
    RasterOcr,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Region {
    rect: [u32; 4],
    text: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Table {
    cells: Vec<Cell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ambiguous_columns: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    row_groups: Vec<RowGroup>,
}

impl Table {
    fn row_group_index(&self, row: u16) -> Option<usize> {
        let index = self.row_groups.partition_point(|group| group.row[1] <= row);
        self.row_groups
            .get(index)
            .filter(|group| group.row[0] <= row)
            .map(|_| index)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RowGroup {
    row: [u16; 2],
    region: u16,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cell {
    // Offsets refer to the ordered, zero-based regions of this page only.
    regions: Vec<u16>,
    row: [u16; 2],
    column: [u16; 2],
    role: CellRole,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CellRole {
    ColumnHeader,
    RowHeader,
    Body,
}

struct RowKeys<'a> {
    by_row: Vec<Vec<&'a Cell>>,
    denied: HashSet<u16>,
}

impl<'a> RowKeys<'a> {
    fn new(page: &Page, table: &'a Table) -> Self {
        let labels = table
            .row_groups
            .iter()
            .map(|group| normalized_label(&page.regions[usize::from(group.region)].text))
            .collect::<Vec<_>>();
        let mut by_row = vec![Vec::new(); 512];
        let mut occurrences = HashMap::<String, Vec<(u16, Option<&str>)>>::new();
        for key in table
            .cells
            .iter()
            .filter(|cell| cell.role == CellRole::RowHeader)
        {
            for row in key.row[0]..key.row[1] {
                by_row[usize::from(row)].push(key);
            }
            let group = table
                .row_group_index(key.row[0])
                .map(|i| labels[i].as_str());
            occurrences
                .entry(normalized_label(&region_text(page, &key.regions)))
                .or_default()
                .push((key.regions[0], group));
        }
        let mut denied = HashSet::new();
        for repeated in occurrences.values().filter(|keys| keys.len() > 1) {
            let mut counts = HashMap::new();
            for group in repeated.iter().filter_map(|&(_, group)| group) {
                *counts.entry(group).or_insert(0usize) += 1;
            }
            for &(region, group) in repeated {
                if group.is_none_or(|label| counts[label] > 1) {
                    denied.insert(region);
                }
            }
        }
        Self { by_row, denied }
    }
}

fn normalized_label(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(super) struct Rendition {
    canonical: Vec<u8>,
    #[cfg(test)]
    text: String,
    regions: Vec<Vec<Range<usize>>>,
    pages: Vec<Page>,
}

impl Rendition {
    fn id(&self, source_hash: &[u8], fingerprint: &[u8]) -> String {
        let mut hash = Sha256::new();
        hash.update(b"rust-rag:ocr-rendition-v1\0");
        hash.update(source_hash);
        hash.update(fingerprint);
        hash.update(&self.canonical);
        format!("{:x}", hash.finalize())
    }

    pub(super) fn passages(self, hash: &str, fingerprint: &str) -> Result<Vec<Passage>> {
        let id = self.id(hash.as_bytes(), fingerprint.as_bytes());
        let mut output = Vec::new();
        let mut citation_bytes = 0;
        for (page, spans) in self.pages.iter().zip(&self.regions) {
            let mut cells = HashMap::new();
            let mut continuation = HashSet::new();
            for (table_number, table) in page.tables.iter().enumerate() {
                let headers = table
                    .cells
                    .iter()
                    .filter(|cell| cell.role == CellRole::ColumnHeader)
                    .collect::<Vec<_>>();
                let row_keys = RowKeys::new(page, table);
                for cell in &table.cells {
                    continuation.extend(cell.regions[1..].iter().copied());
                    let support = if cell.role == CellRole::Body {
                        table_support(
                            page,
                            spans,
                            table,
                            table_number + 1,
                            cell,
                            &row_keys,
                            &headers,
                        )?
                    } else {
                        None
                    };
                    cells.insert(cell.regions[0], (cell.regions.as_slice(), support));
                }
            }
            for index in 0..page.regions.len() as u16 {
                if continuation.contains(&index) {
                    continue;
                }
                let single = [index];
                let (region_ids, support) = cells.remove(&index).unwrap_or((&single, None));
                let first = usize::from(region_ids[0]);
                let last = usize::from(*region_ids.last().expect("nonempty region list"));
                let mut provenance = json!({
                    "schema_version": 1, "rendition_id": id, "extraction_fingerprint": fingerprint,
                    "coordinate_space": "displayed_crop_box_top_left_1000000",
                    "crop_box_mpt": page.crop_box_mpt, "pdf_rotation_cw": page.pdf_rotation_cw,
                    "method": page.method, "regions": region_geometry(page, spans, region_ids),
                });
                let table_terms = match support {
                    Some((support, terms)) => {
                        provenance["table_support"] = support;
                        terms
                    }
                    None => String::new(),
                };
                citation_bytes += serde_json::to_vec(&provenance)?.len();
                ensure!(
                    citation_bytes <= 16 * 1024 * 1024,
                    "OCR provenance exceeds the storage limit."
                );
                output.push(Passage {
                    page: usize::from(page.number),
                    start: spans[first].start,
                    end: spans[last].end,
                    text: region_text(page, region_ids),
                    provenance,
                    table_terms,
                });
            }
        }
        Ok(output)
    }
}

fn table_support(
    page: &Page,
    spans: &[Range<usize>],
    table: &Table,
    number: usize,
    cell: &Cell,
    row_keys: &RowKeys<'_>,
    headers: &[&Cell],
) -> Result<Option<(Value, String)>> {
    let mut keys = row_keys.by_row[usize::from(cell.row[0])]
        .iter()
        .copied()
        .filter(|key| key.row[1] >= cell.row[1] && key.column[1] <= cell.column[0]);
    let Some(key) = keys.next() else {
        return Ok(None);
    };
    // Keep structural ambiguity even when another key fails the label check.
    if keys.next().is_some() || row_keys.denied.contains(&key.regions[0]) {
        return Ok(None);
    }
    let headers = headers
        .iter()
        .copied()
        .filter(|header| {
            header.row[1] <= cell.row[0]
                && header.column[0] <= cell.column[0]
                && header.column[1] >= cell.column[1]
        })
        .take(17)
        .collect::<Vec<_>>();
    if headers.is_empty() {
        return Ok(None);
    }
    ensure!(
        headers.len() <= 16,
        "Too many OCR headers support one cell."
    );
    let ambiguous = table
        .ambiguous_columns
        .iter()
        .any(|column| (cell.column[0]..cell.column[1]).contains(column));
    let row = region_ref(page, spans, &key.regions);
    let row_group = table
        .row_group_index(cell.row[0])
        .map(|i| region_ref(page, spans, &[table.row_groups[i].region]));
    let headers = headers
        .iter()
        .map(|header| region_ref(page, spans, &header.regions))
        .collect::<Vec<_>>();
    let terms = row_group
        .iter()
        .chain(std::iter::once(&row))
        .chain(&headers)
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    ensure!(
        terms.len() <= 8192,
        "OCR table support exceeds the text limit."
    );
    let mut support = json!({"table":number, "row_key":row, "headers":headers,
        "status":if ambiguous { "header_grouping_ambiguous" } else { "grounded" }});
    if let Some(group) = row_group {
        support["row_group"] = group;
    }
    Ok(Some((support, terms)))
}

fn region_geometry(page: &Page, spans: &[Range<usize>], ids: &[u16]) -> Vec<Value> {
    ids.iter()
        .map(|id| {
            let i = usize::from(*id);
            json!({"rect":page.regions[i].rect, "start":spans[i].start, "end":spans[i].end})
        })
        .collect()
}

fn region_text(page: &Page, ids: &[u16]) -> String {
    ids.iter()
        .map(|id| page.regions[usize::from(*id)].text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn region_ref(page: &Page, spans: &[Range<usize>], ids: &[u16]) -> Value {
    json!({"text":region_text(page, ids), "regions":region_geometry(page, spans, ids)})
}

pub(super) fn decode(bytes: &[u8]) -> Result<Rendition> {
    ensure!(
        bytes.len() <= MAX_RESPONSE_BYTES,
        "OCR response exceeds the byte limit"
    );
    // A malformed response may contain transcribed private data. Do not surface it.
    let mut response: Response =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("Invalid OCR response"))?;
    ensure!(response.version == 1, "Unsupported OCR response version");
    ensure!(
        (1..=MAX_PAGES).contains(&response.pages.len()),
        "Invalid OCR page count"
    );
    #[cfg(test)]
    let mut text = String::new();
    let mut regions = Vec::with_capacity(response.pages.len());
    let mut offset = 0;
    let mut total_regions = 0;
    let mut grid_slots = 0;
    for (index, page) in response.pages.iter_mut().enumerate() {
        ensure!(
            usize::from(page.number) == index + 1,
            "Invalid OCR page order"
        );
        let [left, bottom, right, top] = page.crop_box_mpt.map(i64::from);
        ensure!(
            (1..=50_000_000).contains(&(right - left))
                && (1..=50_000_000).contains(&(top - bottom))
                && matches!(page.pdf_rotation_cw, 0 | 90 | 180 | 270),
            "Invalid OCR page geometry"
        );
        total_regions += page.regions.len();
        ensure!(
            total_regions <= MAX_REGIONS,
            "OCR response exceeds the region limit"
        );
        if index != 0 {
            #[cfg(test)]
            text.push_str(PAGE_SEPARATOR);
            offset += PAGE_SEPARATOR.len();
        }
        let mut spans = Vec::with_capacity(page.regions.len());
        for (region_index, region) in page.regions.iter().enumerate() {
            let [x0, y0, x1, y1] = region.rect;
            ensure!(
                x0 < x1 && y0 < y1 && x1 <= COORDINATE_SCALE && y1 <= COORDINATE_SCALE,
                "Invalid OCR region geometry"
            );
            ensure!(
                !region.text.is_empty()
                    && region.text.len() <= 16 * 1024
                    && region.text.trim() == region.text
                    && !region.text.chars().any(|ch| ch.is_control() && ch != '\t'),
                "Invalid OCR region text"
            );
            if region_index != 0 {
                #[cfg(test)]
                text.push('\n');
                offset += 1;
            }
            let end = offset + region.text.chars().count();
            ensure!(
                end <= MAX_CHARACTERS,
                "OCR response exceeds the character limit"
            );
            spans.push(offset..end);
            #[cfg(test)]
            text.push_str(&region.text);
            offset = end;
        }
        validate_tables(&mut page.tables, page.regions.len(), &mut grid_slots)?;
        regions.push(spans);
    }
    ensure!(total_regions > 0, "The PDF has no readable OCR text");
    let canonical = serde_json::to_vec(&response).context("Cannot encode OCR response")?;
    Ok(Rendition {
        canonical,
        #[cfg(test)]
        text,
        regions,
        pages: response.pages,
    })
}

fn validate_tables(
    tables: &mut [Table],
    region_count: usize,
    grid_slots: &mut usize,
) -> Result<()> {
    let mut used_regions = HashSet::new();
    for table in tables.iter_mut() {
        ensure!(!table.cells.is_empty(), "Empty OCR table");
        let mut occupied = HashSet::new();
        for cell in &table.cells {
            let [row_start, row_end] = cell.row;
            let [col_start, col_end] = cell.column;
            ensure!(
                row_start < row_end && row_end <= 512 && col_start < col_end && col_end <= 128,
                "Invalid OCR table cell bounds"
            );
            ensure!(
                (1..=32).contains(&cell.regions.len())
                    && cell
                        .regions
                        .windows(2)
                        .all(|pair| pair[0].checked_add(1) == Some(pair[1]))
                    && cell.regions.iter().all(|region| {
                        usize::from(*region) < region_count && used_regions.insert(*region)
                    }),
                "Invalid OCR table region reference"
            );
            *grid_slots += usize::from(row_end - row_start) * usize::from(col_end - col_start);
            ensure!(
                *grid_slots <= MAX_GRID_SLOTS,
                "OCR response exceeds the table size limit"
            );
            for row in row_start..row_end {
                for column in col_start..col_end {
                    ensure!(
                        occupied.insert((row, column)),
                        "Overlapping OCR table cells"
                    );
                }
            }
        }
        table
            .cells
            .sort_by_key(|cell| (cell.row[0], cell.column[0]));
        validate_row_groups(table, region_count, &mut used_regions)?;
        let columns = table
            .cells
            .iter()
            .map(|cell| cell.column[1])
            .max()
            .unwrap_or(0);
        ensure!(
            table
                .ambiguous_columns
                .iter()
                .all(|column| *column < columns)
                && table
                    .ambiguous_columns
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "Invalid ambiguous OCR columns"
        );
    }
    tables.sort_by_key(|table| table.cells.iter().map(|cell| cell.regions[0]).min());
    Ok(())
}

fn validate_row_groups(
    table: &Table,
    region_count: usize,
    used_regions: &mut HashSet<u16>,
) -> Result<()> {
    if table.row_groups.is_empty() {
        return Ok(());
    }
    let header_end = table
        .cells
        .iter()
        .filter(|cell| cell.role == CellRole::ColumnHeader)
        .map(|cell| cell.row[1])
        .max()
        .unwrap_or(0);
    let body_end = table
        .cells
        .iter()
        .filter(|cell| cell.role != CellRole::ColumnHeader)
        .map(|cell| cell.row[1])
        .max()
        .unwrap_or(0);
    let mut cells = table.cells.iter().peekable();
    let mut previous_end = header_end;
    let mut last_region = None;
    for group in &table.row_groups {
        let [start, end] = group.row;
        ensure!(
            previous_end <= start && start < end && end <= body_end,
            "Invalid OCR row group bounds"
        );
        ensure!(
            usize::from(group.region) < region_count && used_regions.insert(group.region),
            "Invalid OCR row group region reference"
        );
        while let Some(cell) = cells.next_if(|cell| cell.row[0] < start) {
            ensure!(
                cell.row[1] <= start,
                "OCR cell crosses a row group boundary"
            );
            last_region = last_region.max(cell.regions.last().copied());
        }
        ensure!(
            last_region.is_none_or(|region| region < group.region),
            "Invalid OCR row group region order"
        );
        let mut first_region = None;
        while let Some(cell) = cells.next_if(|cell| cell.row[0] < end) {
            ensure!(cell.row[1] <= end, "OCR cell crosses a row group boundary");
            ensure!(
                cell.regions[0] > group.region,
                "Invalid OCR row group region order"
            );
            if cell.row[0] == start {
                first_region = Some(
                    first_region.map_or(cell.regions[0], |region: u16| region.min(cell.regions[0])),
                );
            }
            last_region = last_region.max(cell.regions.last().copied());
        }
        // Use canonical region order. Displayed coordinates may be rotated.
        ensure!(
            first_region == group.region.checked_add(1),
            "Invalid OCR row group region order"
        );
        previous_end = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn region(text: &str) -> Value {
        json!({"rect":[100, 200, 10000, 20000], "text":text})
    }

    fn fixture() -> Value {
        json!({"version":1, "pages":[
            {"number":1, "crop_box_mpt":[-1000, 0, 600000, 800000],
             "pdf_rotation_cw":90, "method":"raster_ocr", "regions":[
                region("Gauge"), region("North"), region("e\u{301}😀"), region("kPa")
             ], "tables":[{"cells":[
                {"regions":[2, 3], "row":[1, 2], "column":[1, 2], "role":"body"},
                {"regions":[0], "row":[0, 1], "column":[1, 2], "role":"column_header"},
                {"regions":[1], "row":[1, 2], "column":[0, 1], "role":"row_header"}
             ]}]},
            {"number":2, "crop_box_mpt":[0, 0, 600000, 800000],
             "pdf_rotation_cw":0, "method":"text_layer", "regions":[], "tables":[]},
            {"number":3, "crop_box_mpt":[0, 0, 600000, 800000],
             "pdf_rotation_cw":0, "method":"text_layer", "regions":[region("Final")], "tables":[]}
        ]})
    }

    fn row_groups_fixture(groups: &[(&str, &[&str])], unscoped: &[&str]) -> Value {
        let mut regions = vec![region("Gauge")];
        let mut cells = vec![json!({"regions":[0], "row":[0, 1], "column":[1, 2],
                                    "role":"column_header"})];
        let mut row_groups = Vec::new();
        let mut row = 1;
        for (label, keys) in groups
            .iter()
            .map(|(label, keys)| (Some(*label), *keys))
            .chain(std::iter::once((None, unscoped)))
        {
            let start = row;
            let label_region = label.map(|label| {
                let index = regions.len();
                regions.push(region(label));
                index
            });
            for key in keys {
                let index = regions.len();
                regions.push(region(key));
                regions.push(region(&format!("value {row}")));
                cells.push(
                    json!({"regions":[index], "row":[row, row+1], "column":[0, 1],
                                  "role":"row_header"}),
                );
                cells.push(
                    json!({"regions":[index+1], "row":[row, row+1], "column":[1, 2],
                                  "role":"body"}),
                );
                row += 1;
            }
            if let Some(index) = label_region {
                row_groups.push(json!({"row":[start, row], "region":index}));
            }
        }
        json!({"version":1, "pages":[{
            "number":1, "crop_box_mpt":[0, 0, 600000, 800000],
            "pdf_rotation_cw":90, "method":"raster_ocr", "regions":regions,
            "tables":[{"cells":cells, "row_groups":row_groups}]
        }]})
    }

    fn passages(wire: &Value) -> Result<Vec<Passage>> {
        decode(wire.to_string().as_bytes())?.passages(&"a".repeat(64), &"b".repeat(64))
    }

    fn find<'a>(passages: &'a [Passage], text: &str) -> &'a Passage {
        passages
            .iter()
            .find(|passage| passage.text == text)
            .unwrap()
    }

    #[test]
    fn preserves_exact_unicode_and_page_boundaries() -> Result<()> {
        let actual = decode(&fixture().to_string().into_bytes())?;
        assert_eq!(actual.text, "Gauge\nNorth\ne\u{301}😀\nkPa\n\n\n\nFinal");
        assert_eq!(
            actual.regions[..2],
            [vec![0..5, 6..11, 12..15, 16..19], vec![]]
        );
        assert_eq!(
            actual.regions[2].as_slice(),
            std::slice::from_ref(&(23..28))
        );
        let chars: Vec<_> = actual.text.chars().collect();
        assert_eq!(
            chars[actual.regions[0][2].clone()]
                .iter()
                .collect::<String>(),
            "e\u{301}😀"
        );
        Ok(())
    }

    #[test]
    fn hashes_canonical_geometry_and_source_identity() -> Result<()> {
        let mut wire = fixture();
        let first = decode(wire.to_string().as_bytes())?;
        wire["pages"][0]["tables"][0]["cells"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let shuffled = decode(serde_json::to_string_pretty(&wire)?.as_bytes())?;
        assert_eq!(first.canonical, shuffled.canonical);
        wire["pages"][0]["tables"][0]["row_groups"] = json!([]);
        assert_eq!(
            first.canonical,
            decode(wire.to_string().as_bytes())?.canonical
        );
        let id = first.id(&[0; 32], &[1; 32]);
        assert_eq!(id, shuffled.id(&[0; 32], &[1; 32]));
        assert_ne!(id, first.id(&[1; 32], &[1; 32]));
        assert_ne!(id, first.id(&[0; 32], &[2; 32]));
        wire["pages"][0]["regions"][2]["rect"][0] = json!(300);
        assert_ne!(
            id,
            decode(wire.to_string().as_bytes())?.id(&[0; 32], &[1; 32])
        );
        Ok(())
    }

    #[test]
    fn distinct_row_groups_ground_equal_normalized_keys() -> Result<()> {
        let wire = row_groups_fixture(
            &[("Dock East", &["PUMP\tA"]), ("Dock West", &["pump A"])],
            &[],
        );
        let rendered = passages(&wire)?;
        for (body, label, key) in [
            ("value 1", "Dock East", "PUMP\tA"),
            ("value 2", "Dock West", "pump A"),
        ] {
            let passage = find(&rendered, body);
            let group = find(&rendered, label);
            let support = &passage.provenance["table_support"];
            assert_eq!(support["row_group"]["text"], label);
            assert_eq!(support["row_group"]["regions"], group.provenance["regions"]);
            assert_eq!(support["row_key"]["text"], key);
            assert_eq!(support["headers"][0]["text"], "Gauge");
            assert_eq!(passage.table_terms, format!("{label} {key} Gauge"));
            assert!(group.provenance.get("table_support").is_none());
        }
        Ok(())
    }

    #[test]
    fn repeated_or_unscoped_labels_withhold_only_ambiguous_support() -> Result<()> {
        let rendered = passages(&row_groups_fixture(
            &[
                ("Dock  East", &["Pump A", "pump  A"]),
                ("dock east", &["PUMP\tA"]),
                ("Dock West", &["pump a", "Pump B"]),
            ],
            &["pump a", "Pump C"],
        ))?;
        for text in ["value 1", "value 2", "value 3", "value 6"] {
            let passage = find(&rendered, text);
            assert!(passage.provenance.get("table_support").is_none());
            assert!(passage.table_terms.is_empty());
        }
        for text in ["value 4", "value 5"] {
            assert_eq!(
                find(&rendered, text).provenance["table_support"]["row_group"]["text"],
                "Dock West"
            );
        }
        let unique = find(&rendered, "value 7");
        assert_eq!(unique.table_terms, "Pump C Gauge");
        assert!(
            unique.provenance["table_support"]
                .get("row_group")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn structural_key_ambiguity_survives_textual_disqualification() -> Result<()> {
        let mut wire = row_groups_fixture(&[], &["Pump A", "pump a"]);
        wire["pages"][0]["regions"]
            .as_array_mut()
            .unwrap()
            .push(region("Valve"));
        let cells = wire["pages"][0]["tables"][0]["cells"]
            .as_array_mut()
            .unwrap();
        cells[0]["column"] = json!([2, 3]);
        cells[2]["column"] = json!([2, 3]);
        cells.push(json!({"regions":[5], "row":[1, 2], "column":[1, 2], "role":"row_header"}));
        let rendered = passages(&wire)?;
        assert!(
            find(&rendered, "value 1")
                .provenance
                .get("table_support")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn validates_row_group_bounds_references_and_canonical_order() -> Result<()> {
        let fixture = row_groups_fixture(&[("East", &["A", "B"]), ("West", &["C"])], &[]);
        for (pointer, value) in [
            ("/pages/0/tables/0/row_groups/0/row", json!([0, 3])),
            ("/pages/0/tables/0/row_groups/0/row", json!([1, 1])),
            ("/pages/0/tables/0/row_groups/0/row", json!([1, 4])),
            ("/pages/0/tables/0/row_groups/1/row", json!([3, 5])),
            ("/pages/0/tables/0/row_groups/0/row", json!([2, 3])),
            ("/pages/0/tables/0/row_groups/0/region", json!(2)),
            ("/pages/0/tables/0/row_groups/1/region", json!(1)),
            ("/pages/0/tables/0/row_groups/1/region", json!(9)),
        ] {
            let mut wire = fixture.clone();
            *wire.pointer_mut(pointer).unwrap() = value;
            assert!(decode(wire.to_string().as_bytes()).is_err(), "{pointer}");
        }
        let mut wire = fixture.clone();
        wire["pages"][0]["tables"][0]["row_groups"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(decode(wire.to_string().as_bytes()).is_err());

        let mut wire = fixture.clone();
        wire["pages"][0]["tables"]
            .as_array_mut()
            .unwrap()
            .push(json!({"cells":[
                {"regions":[1], "row":[0, 1], "column":[0, 1], "role":"row_header"}
            ]}));
        assert!(decode(wire.to_string().as_bytes()).is_err());

        let mut wire = fixture.clone();
        wire["pages"][0]["regions"]
            .as_array_mut()
            .unwrap()
            .push(region("Later"));
        wire["pages"][0]["tables"][0]["cells"][0]["regions"] = json!([9]);
        assert!(decode(wire.to_string().as_bytes()).is_err());

        let mut wire = fixture.clone();
        let cells = wire["pages"][0]["tables"][0]["cells"]
            .as_array_mut()
            .unwrap();
        cells[4]["row"] = json!([2, 4]);
        cells[6]["column"] = json!([2, 3]);
        assert!(decode(wire.to_string().as_bytes()).is_err());

        let mut wire = fixture;
        wire["pages"][0]["tables"][0]["cells"]
            .as_array_mut()
            .unwrap()
            .remove(3);
        let rendered = passages(&wire)?;
        assert!(
            find(&rendered, "value 2")
                .provenance
                .get("table_support")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn rejects_untrusted_or_ambiguous_wire_shape() {
        for (pointer, value) in [
            ("/pages/1/number", json!(1)),
            ("/pages/0/pdf_rotation_cw", json!(45)),
            ("/pages/0/crop_box_mpt", json!([0, 0, 0, 1000])),
            ("/pages/0/regions/0/rect", json!([1, 2, 1, 3])),
            ("/pages/0/regions/0/rect", json!([1, 2, 3, 1000001])),
            ("/pages/0/regions/0/text", json!("private\0string")),
            ("/pages/0/tables/0/cells/0/regions", json!([2, 4])),
            ("/pages/0/tables/0/cells/0/regions", json!([0])),
            ("/pages/0/tables/0/cells/0/row", json!([0, 2])),
        ] {
            let mut wire = fixture();
            *wire.pointer_mut(pointer).unwrap() = value;
            let error = decode(wire.to_string().as_bytes()).err().unwrap();
            assert!(!error.to_string().contains("private"), "{pointer}");
        }
        let mut wire = fixture();
        wire["pages"][0]["regions"][0]["path"] = json!("private/path");
        assert_eq!(
            decode(wire.to_string().as_bytes())
                .err()
                .unwrap()
                .to_string(),
            "Invalid OCR response"
        );
        assert!(decode(&vec![b' '; MAX_RESPONSE_BYTES + 1]).is_err());
    }

    #[test]
    fn wrapped_cells_keep_exact_support_and_ambiguous_parents_are_labeled() -> Result<()> {
        let mut wire = fixture();
        wire["pages"][0]["tables"][0]["ambiguous_columns"] = json!([1]);
        let rendered =
            decode(wire.to_string().as_bytes())?.passages(&"a".repeat(64), &"b".repeat(64))?;
        let cell = &rendered[2];
        assert_eq!(cell.text, "e\u{301}😀\nkPa");
        assert_eq!((cell.page, cell.start, cell.end), (1, 12, 19));
        assert_eq!(cell.table_terms, "North Gauge");
        assert_eq!(cell.provenance["regions"].as_array().unwrap().len(), 2);
        assert_eq!(cell.provenance["table_support"]["row_key"]["text"], "North");
        assert_eq!(
            cell.provenance["table_support"]["headers"][0]["text"],
            "Gauge"
        );
        assert_eq!(
            cell.provenance["table_support"]["status"],
            "header_grouping_ambiguous"
        );
        Ok(())
    }
}
