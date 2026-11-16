//! Binary Excel adapters. XLSB uses sparse cell readers, never dense ranges.
use super::{Cell, CellValue, ParseOptions, Sheet, Workbook};
use anyhow::{anyhow, ensure, Context, Result};
use calamine::{Data, DataType, Reader, SheetType, Xls, Xlsb};
use ooxml_archive::OoxmlArchive;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

const CFB_MAGIC: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

pub(super) fn is_xls(bytes: &[u8]) -> bool {
    bytes.starts_with(CFB_MAGIC)
}

pub(super) fn is_xls_path(path: &Path) -> Result<bool> {
    let mut head = Vec::with_capacity(8);
    std::fs::File::open(path)?.take(8).read_to_end(&mut head)?;
    Ok(is_xls(&head))
}

pub(super) fn is_xlsb(archive: &OoxmlArchive) -> bool {
    archive
        .part_names()
        .any(|name| name.eq_ignore_ascii_case("xl/workbook.bin"))
}

pub(super) fn parse_xls_path(path: &Path, options: ParseOptions) -> Result<Workbook> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(options.max_inflate.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    parse_xls(&bytes, options)
}

fn guarded<T>(run: impl FnOnce() -> Result<T>) -> Result<T> {
    // Malformed binary records must not unwind through Python/Node/Flight.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .map_err(|_| anyhow!("malformed binary Excel workbook"))?
}

pub(super) fn parse_xls(bytes: &[u8], options: ParseOptions) -> Result<Workbook> {
    ensure!(
        bytes.len() <= options.max_inflate,
        "XLS input exceeds max_inflate"
    );
    preflight_xls(bytes, options.max_inflate)?;
    guarded(|| {
        let mut book = Xls::new(Cursor::new(bytes)).context("read XLS workbook")?;
        let date_1904 = book.has_1904_epoch();
        let metadata = book.sheets_metadata().to_vec();
        let mut sheets = Vec::new();
        let mut retained = 0usize;
        for (index, sheet) in metadata.into_iter().enumerate() {
            if sheet.typ != SheetType::WorkSheet {
                continue;
            }
            let values = book.worksheet_range(&sheet.name)?;
            let mut cells = BTreeMap::new();
            let (row_start, col_start) = values.start().unwrap_or((0, 0));
            for (row, col, value) in values.used_cells() {
                let cell = Cell {
                    row: row_start + row as u32 + 1,
                    col: col_start + col as u32 + 1,
                    formula: None,
                    value: convert(value, options, date_1904)?,
                };
                charge_cell(&mut retained, &cell, options.max_inflate)?;
                cells.insert((cell.row, cell.col), cell);
            }
            if options.include_formulas {
                let formulas = book.worksheet_formula(&sheet.name)?;
                let (row_start, col_start) = formulas.start().unwrap_or((0, 0));
                for (row, col, formula) in formulas.used_cells() {
                    ensure!(
                        !formula.starts_with("Unrecognised formula"),
                        "unsupported XLS formula: {formula}"
                    );
                    attach_formula(
                        &mut cells,
                        row_start + row as u32 + 1,
                        col_start + col as u32 + 1,
                        formula.clone(),
                        &mut retained,
                        options.max_inflate,
                    )?;
                }
            }
            sheets.push(Sheet {
                id: (index + 1) as u32,
                name: sheet.name,
                cells: cells.into_values().collect(),
            });
        }
        Ok(Workbook { sheets })
    })
}

pub(super) fn parse_xlsb(archive: &OoxmlArchive, options: ParseOptions) -> Result<Workbook> {
    // Validate checksums and the aggregate actual expansion before Calamine's
    // ZIP reader can allocate workbook metadata/shared strings or read cells.
    for name in archive.part_names() {
        let part = archive
            .read_part(name, options.max_inflate)
            .with_context(|| format!("validate XLSB part {name}"))?;
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "xl/workbook.bin" | "xl/styles.bin" | "xl/sharedstrings.bin"
        ) || (lower.starts_with("xl/worksheets/") && lower.ends_with(".bin"))
        {
            preflight_xlsb_records(&part)
                .with_context(|| format!("validate XLSB records in {name}"))?;
        }
    }
    guarded(|| {
        let mut book = Xlsb::new(Cursor::new(archive.as_bytes())).context("read XLSB workbook")?;
        let date_1904 = book.has_1904_epoch();
        let metadata = book.sheets_metadata().to_vec();
        let mut sheets = Vec::new();
        let mut retained = 0usize;
        for (index, sheet) in metadata.into_iter().enumerate() {
            if sheet.typ != SheetType::WorkSheet {
                continue;
            }
            let mut cells = BTreeMap::new();
            let mut reader = book.worksheet_cells_reader(&sheet.name)?;
            while let Some(source) = reader.next_cell()? {
                let (row, col) = source.get_position();
                ensure!(
                    row < 1_048_576 && col < 16_384,
                    "XLSB cell outside worksheet limits"
                );
                let cell = Cell {
                    row: row + 1,
                    col: col + 1,
                    formula: None,
                    value: convert(source.get_value(), options, date_1904)?,
                };
                charge_cell(&mut retained, &cell, options.max_inflate)?;
                ensure!(
                    cells.insert((cell.row, cell.col), cell).is_none(),
                    "duplicate XLSB cell"
                );
            }
            drop(reader);
            if options.include_formulas {
                let mut reader = book.worksheet_cells_reader(&sheet.name)?;
                while let Some(source) = reader.next_formula()? {
                    let (row, col) = source.get_position();
                    ensure!(
                        row < 1_048_576 && col < 16_384,
                        "XLSB formula outside worksheet limits"
                    );
                    attach_formula(
                        &mut cells,
                        row + 1,
                        col + 1,
                        source.get_value().clone(),
                        &mut retained,
                        options.max_inflate,
                    )?;
                }
            }
            sheets.push(Sheet {
                id: (index + 1) as u32,
                name: sheet.name,
                cells: cells.into_values().collect(),
            });
        }
        Ok(Workbook { sheets })
    })
}

fn attach_formula(
    cells: &mut BTreeMap<(u32, u32), Cell>,
    row: u32,
    col: u32,
    formula: String,
    retained: &mut usize,
    limit: usize,
) -> Result<()> {
    charge(retained, formula.len(), limit)?;
    if let std::collections::btree_map::Entry::Vacant(entry) = cells.entry((row, col)) {
        let cell = Cell {
            row,
            col,
            formula: None,
            value: CellValue::Empty,
        };
        charge_cell(retained, &cell, limit)?;
        entry.insert(cell);
    }
    cells.get_mut(&(row, col)).unwrap().formula = Some(formula);
    Ok(())
}

fn convert(value: &impl DataType, options: ParseOptions, date_1904: bool) -> Result<CellValue> {
    if value.is_empty() {
        return Ok(CellValue::Empty);
    }
    if let Some(text) = value
        .get_string()
        .or_else(|| value.get_datetime_iso())
        .or_else(|| value.get_duration_iso())
    {
        return Ok(CellValue::String { v: Arc::from(text) });
    }
    if let Some(value) = value.get_bool() {
        return Ok(if options.raw_values {
            CellValue::Raw {
                v: Arc::from(if value { "1" } else { "0" }),
            }
        } else {
            CellValue::Bool { v: value }
        });
    }
    if let Some(error) = value.get_error() {
        let v = Arc::from(error.to_string());
        return Ok(if options.raw_values {
            CellValue::Raw { v }
        } else {
            CellValue::Error { v }
        });
    }
    let date = value.get_datetime();
    let number = date
        .map(|d| d.as_f64())
        .or_else(|| value.get_float())
        .or_else(|| value.get_int().map(|n| n as f64))
        .ok_or_else(|| anyhow!("unsupported binary Excel cell value"))?;
    ensure!(number.is_finite(), "non-finite binary Excel number");
    if options.raw_values {
        return Ok(CellValue::Raw {
            v: Arc::from(number.to_string()),
        });
    }
    if options.decode_dates && date.is_some_and(|date| date.is_datetime()) {
        if let Some(iso) = super::excel_serial_to_iso(number, date_1904) {
            return Ok(CellValue::Date {
                iso,
                serial: number,
            });
        }
    }
    Ok(CellValue::Number { v: number })
}

fn charge(retained: &mut usize, bytes: usize, limit: usize) -> Result<()> {
    *retained = retained
        .checked_add(bytes)
        .ok_or_else(|| anyhow!("binary workbook budget overflow"))?;
    ensure!(
        *retained <= limit,
        "binary workbook retained cells exceed max_inflate"
    );
    Ok(())
}

fn charge_cell(retained: &mut usize, cell: &Cell, limit: usize) -> Result<()> {
    let text = match &cell.value {
        CellValue::String { v } | CellValue::Raw { v } | CellValue::Error { v } => v.len(),
        CellValue::Date { iso, .. } => iso.len(),
        _ => 0,
    };
    charge(retained, std::mem::size_of::<Cell>() + 64 + text, limit)
}

// Calamine's XLS reader builds dense value/formula ranges and reserves from
// DIMENSIONS. Bound both declared and actual occupied extents before it runs.
fn preflight_xls(bytes: &[u8], limit: usize) -> Result<()> {
    let mut cfb = cfb::CompoundFile::open(Cursor::new(bytes)).context("read XLS compound file")?;
    let path = cfb
        .walk()
        .find(|entry| {
            entry.is_stream()
                && (entry.name().eq_ignore_ascii_case("Workbook")
                    || entry.name().eq_ignore_ascii_case("Book"))
        })
        .map(|entry| entry.path().to_owned())
        .ok_or_else(|| {
            anyhow!("missing XLS Workbook stream (encrypted workbooks are unsupported)")
        })?;
    let mut stream = Vec::new();
    cfb.open_stream(path)?
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut stream)?;
    ensure!(
        stream.len() <= limit,
        "XLS Workbook stream exceeds max_inflate"
    );
    preflight_biff(&stream, limit)
}

fn record<'a>(stream: &mut &'a [u8]) -> Result<(u16, &'a [u8])> {
    ensure!(stream.len() >= 4, "truncated XLS record header");
    let kind = u16::from_le_bytes([stream[0], stream[1]]);
    let len = u16::from_le_bytes([stream[2], stream[3]]) as usize;
    ensure!(stream.len() >= 4 + len, "truncated XLS record payload");
    let data = &stream[4..4 + len];
    *stream = &stream[4 + len..];
    Ok((kind, data))
}

fn preflight_biff(bytes: &[u8], limit: usize) -> Result<()> {
    let mut globals = bytes;
    let mut offsets = Vec::new();
    loop {
        let (kind, data) = record(&mut globals)?;
        if kind == 0x0085 {
            ensure!(data.len() >= 4, "truncated XLS BoundSheet record");
            offsets.push(u32::from_le_bytes(data[..4].try_into()?) as usize);
        }
        if kind == 0x000A {
            break;
        }
    }
    let mut total = 0usize;
    for offset in offsets {
        let mut stream = bytes
            .get(offset..)
            .ok_or_else(|| anyhow!("invalid XLS sheet offset"))?;
        let mut bounds = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut declared = 0usize;
        let mut source_cells = 0usize;
        let mut depth = 0usize;
        loop {
            let (kind, data) = record(&mut stream)?;
            if kind == 0x0809 {
                depth += 1;
            }
            if kind == 0x000A {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            if kind == 0x0207 {
                source_cells = source_cells.saturating_add(1);
            }
            if kind == 0x0200 {
                let (first_row, end_row, first_col, end_col) = match data.len() {
                    10 => (
                        u16_at(data, 0)? as u32,
                        u16_at(data, 2)? as u32,
                        u16_at(data, 4)? as u32,
                        u16_at(data, 6)? as u32,
                    ),
                    14 | 16 => (
                        u32::from_le_bytes(data[0..4].try_into()?),
                        u32::from_le_bytes(data[4..8].try_into()?),
                        u16_at(data, 8)? as u32,
                        u16_at(data, 10)? as u32,
                    ),
                    _ => return Err(anyhow!("invalid XLS DIMENSIONS length")),
                };
                ensure!(
                    end_row <= 65536
                        && end_col <= 256
                        && end_row >= first_row
                        && end_col >= first_col,
                    "invalid XLS dimensions"
                );
                declared = declared.max(
                    ((end_row - first_row) as usize).saturating_mul((end_col - first_col) as usize),
                );
            }
            if matches!(
                kind,
                0x0203 | 0x0204 | 0x00D6 | 0x0205 | 0x027E | 0x00FD | 0x00BD | 0x0006
            ) {
                let row = u16_at(data, 0)? as u32;
                let col = u16_at(data, 2)? as u32;
                let last_col = if kind == 0x00BD {
                    u16_at(
                        data,
                        data.len()
                            .checked_sub(2)
                            .ok_or_else(|| anyhow!("truncated MulRk"))?,
                    )? as u32
                } else {
                    col
                };
                ensure!(col <= last_col && last_col < 256, "invalid XLS cell column");
                source_cells = source_cells.saturating_add((last_col - col + 1) as usize);
                bounds.0 = bounds.0.min(row);
                bounds.1 = bounds.1.min(col);
                bounds.2 = bounds.2.max(row);
                bounds.3 = bounds.3.max(last_col);
            }
        }
        let occupied = if bounds.0 == u32::MAX {
            0
        } else {
            (bounds.2 - bounds.0 + 1) as usize * (bounds.3 - bounds.1 + 1) as usize
        };
        // Includes the sparse source cells, dense values/formulas and temporary
        // range copies retained by the XLS backend.
        let estimate = declared
            .max(occupied)
            .max(source_cells)
            .saturating_mul(4 * std::mem::size_of::<Data>() + 2 * std::mem::size_of::<String>());
        charge(&mut total, estimate, limit).context("XLS dense range allocation budget")?;
    }
    Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let pair = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| anyhow!("truncated XLS cell record"))?;
    Ok(u16::from_le_bytes(pair.try_into()?))
}

// Validate record framing before a dependency resizes buffers using record
// lengths. A tiny compressed file must not request a 256 MiB record buffer.
fn preflight_xlsb_records(mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let kind = read_varint(&mut bytes, 2)?;
        let length = read_varint(&mut bytes, 4)? as usize;
        let data = bytes
            .get(..length)
            .ok_or_else(|| anyhow!("truncated XLSB record"))?;
        if kind == 0x016A {
            ensure!(data.len() >= 4, "truncated XLSB external sheet count");
            let count = u32::from_le_bytes(data[..4].try_into()?) as usize;
            ensure!(
                count <= (data.len() - 4) / 12,
                "invalid XLSB external sheet count"
            );
        }
        bytes = &bytes[length..];
    }
    Ok(())
}

fn read_varint(bytes: &mut &[u8], max_bytes: usize) -> Result<u32> {
    let mut value = 0;
    for shift in 0..max_bytes {
        let (&byte, tail) = bytes
            .split_first()
            .ok_or_else(|| anyhow!("truncated XLSB record header"))?;
        *bytes = tail;
        value |= u32::from(byte & 0x7f) << (7 * shift);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(anyhow!("invalid XLSB variable-length integer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversized_and_truncated_xlsb_records_before_allocation() {
        assert!(preflight_xlsb_records(&[0, 0xff, 0xff, 0xff, 0x7f]).is_err());
        assert!(preflight_xlsb_records(&[0x80]).is_err());
        assert!(preflight_xlsb_records(&[0, 0x80, 0x80, 0x80, 0x80]).is_err());
        assert!(preflight_xlsb_records(&[0, 0]).is_ok());
    }

    fn biff_record(kind: u16, data: &[u8], out: &mut Vec<u8>) {
        out.extend(kind.to_le_bytes());
        out.extend((data.len() as u16).to_le_bytes());
        out.extend(data);
    }

    #[test]
    fn rejects_sparse_and_declared_xls_allocation_bombs() {
        for declared in [false, true] {
            let mut stream = Vec::new();
            biff_record(0x0085, &12u32.to_le_bytes(), &mut stream);
            biff_record(0x000A, &[], &mut stream);
            biff_record(0x0809, &[], &mut stream);
            if declared {
                let mut dims = vec![0; 14];
                dims[4..8].copy_from_slice(&65536u32.to_le_bytes());
                dims[10..12].copy_from_slice(&256u16.to_le_bytes());
                biff_record(0x0200, &dims, &mut stream);
            } else {
                biff_record(0x0203, &[0, 0, 0, 0], &mut stream);
                // An embedded substream must not terminate the outer preflight.
                biff_record(0x0809, &[], &mut stream);
                biff_record(0x000A, &[], &mut stream);
                biff_record(0x0203, &[255, 255, 255, 0], &mut stream);
            }
            biff_record(0x000A, &[], &mut stream);
            assert!(preflight_biff(&stream, 128 * 1024 * 1024).is_err());
        }
    }
}
