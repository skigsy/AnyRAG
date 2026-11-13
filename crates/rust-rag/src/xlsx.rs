//! Bounded Excel cell extraction in the existing isolated document worker.

use crate::document_worker;
use anyhow::{Context, Result, ensure};
use calamine::{Data, DataRef, Reader, SheetVisible, Xlsx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Cursor;
use zip::ZipArchive;

const MAX_EXPANDED: u128 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_CELLS: usize = 20_000;
const MAX_TEXT: usize = 2_000_000;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const INVALID: &str = "The XLSX package could not be read safely.";
const LIMIT: &str = "The XLSX exceeds the reader's archive, cell or text limits.";

type ParseResult<T> = std::result::Result<T, &'static str>;

#[derive(Deserialize, Serialize)]
pub struct Cell {
    pub anchor: String,
    pub text: String,
}

pub fn extract(raw: &[u8]) -> Result<Vec<Cell>> {
    ensure!(raw.len() <= document_worker::MAX_INPUT, "{LIMIT}");
    let response = document_worker::run(raw, "__xlsx-worker", MAX_OUTPUT)?;
    let mut value: Value = serde_json::from_slice(&response).context(INVALID)?;
    ensure!(value["schema_version"] == 1, "{INVALID}");
    if let Some(error) = value["error"].as_str() {
        ensure!(matches!(error, INVALID | LIMIT), "{INVALID}");
        anyhow::bail!("{error}");
    }
    let cells: Vec<Cell> = serde_json::from_value(value["cells"].take()).context(INVALID)?;
    ensure!(cells.len() <= MAX_CELLS, "{LIMIT}");
    ensure!(
        cells.iter().map(|cell| cell.text.len()).sum::<usize>() <= MAX_TEXT,
        "{LIMIT}"
    );
    ensure!(
        cells.iter().all(|cell| cell.anchor.starts_with("sheet:")
            && cell.anchor.len() <= 160
            && !cell.text.contains('\0')),
        "{INVALID}"
    );
    Ok(cells)
}

pub fn worker() -> Result<()> {
    document_worker::respond(|raw| {
        parse(raw).map(|cells| json!({"schema_version": 1, "cells": cells}))
    })
}

fn parse(raw: &[u8]) -> ParseResult<Vec<Cell>> {
    if raw.len() > document_worker::MAX_INPUT {
        return Err(LIMIT);
    }
    let mut archive = ZipArchive::new(Cursor::new(raw)).map_err(|_| INVALID)?;
    if archive.len() > MAX_ENTRIES || archive.decompressed_size().is_none_or(|n| n > MAX_EXPANDED) {
        return Err(LIMIT);
    }
    archive.validate_header_consistency().map_err(|_| INVALID)?;
    for index in 0..archive.len() {
        let entry = archive.by_index_data(index).map_err(|_| INVALID)?;
        if entry.encrypted()
            || entry.is_symlink()
            || entry
                .name()
                .map_err(|_| INVALID)?
                .to_ascii_lowercase()
                .ends_with("/vbaproject.bin")
        {
            return Err(INVALID);
        }
    }
    drop(archive);
    let mut book = Xlsx::new(Cursor::new(raw)).map_err(|_| INVALID)?;
    let names: Vec<_> = book
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.visible == SheetVisible::Visible)
        .map(|sheet| sheet.name.clone())
        .collect();
    let mut cells = Vec::new();
    let mut bytes = 0;
    for sheet in names {
        let mut reader = book.worksheet_cells_reader(&sheet).map_err(|_| INVALID)?;
        while let Some(cell) = reader
            .next_cell_with_formula_metadata()
            .map_err(|_| INVALID)?
        {
            let value = cell.value;
            if cell.formula.is_some() || matches!(value, DataRef::Error(_) | DataRef::Empty) {
                continue;
            }
            let text = Data::from(value).to_string();
            if text.trim().is_empty() {
                continue;
            }
            bytes += text.len();
            if bytes > MAX_TEXT || cells.len() >= MAX_CELLS {
                return Err(LIMIT);
            }
            let address = cell_address(cell.pos);
            cells.push(Cell {
                anchor: format!("sheet:{sheet}!{address}"),
                text,
            });
        }
    }
    Ok(cells)
}

fn cell_address((row, column): (u32, u32)) -> String {
    let mut column = column + 1;
    let mut letters = Vec::with_capacity(3);
    while column > 0 {
        column -= 1;
        letters.push(char::from(b'A' + (column % 26) as u8));
        column /= 26;
    }
    format!(
        "{}{}",
        letters.into_iter().rev().collect::<String>(),
        row + 1
    )
}
