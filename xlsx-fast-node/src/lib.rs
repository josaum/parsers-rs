// Rust guideline compliant 2026-02-21
//! Node-API bindings for `xlsx-fast-core`.
//!
//! Every function takes a file path, mirrors the Python binding one-for-one,
//! and reports parse failures as JavaScript errors. Options objects are
//! partial: an omitted field takes the core crate's default.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use xlsx_fast_core::arrow::{parse_xlsx_to_arrow_ipc, ArrowOptions};
use xlsx_fast_core::{parse_xlsx, parse_xlsx_to_ndjson, ParseOptions};

/// Parse options exposed to Node.js consumers.
#[napi(object)]
pub struct ParseOptionsJs {
    /// Decode date-formatted serials to ISO-8601 strings (default: true).
    pub decode_dates: Option<bool>,
    /// Include cell formulas alongside values (default: false).
    pub include_formulas: Option<bool>,
    /// Return raw cell values without number-format decoding (default: false).
    pub raw_values: Option<bool>,
    /// Aggregate inflate budget for the archive, in bytes (default: 128 MiB).
    pub max_inflate: Option<u32>,
}

/// Arrow output options exposed to Node.js consumers.
#[napi(object)]
pub struct ArrowOptionsJs {
    /// Cells per Arrow record batch (default: the core crate's).
    pub batch_size: Option<u32>,
}

/// Build [`ParseOptions`] from a partial JS object, filling unset fields from the core default.
fn to_parse_options(opts: Option<ParseOptionsJs>) -> ParseOptions {
    let d = ParseOptions::default();
    match opts {
        None => d,
        Some(o) => ParseOptions {
            decode_dates: o.decode_dates.unwrap_or(d.decode_dates),
            include_formulas: o.include_formulas.unwrap_or(d.include_formulas),
            raw_values: o.raw_values.unwrap_or(d.raw_values),
            max_inflate: o.max_inflate.map_or(d.max_inflate, |v| v as usize),
        },
    }
}

/// Build [`ArrowOptions`] from a partial JS object, filling unset fields from the core default.
fn to_arrow_options(opts: Option<ArrowOptionsJs>) -> ArrowOptions {
    let d = ArrowOptions::default();
    ArrowOptions {
        batch_size: opts
            .and_then(|o| o.batch_size)
            .map_or(d.batch_size, |v| v as usize),
        ..d
    }
}

/// Convert a core error into a JavaScript error carrying its message.
fn js_error(err: impl std::fmt::Display) -> Error {
    Error::from_reason(err.to_string())
}

/// Parse an XLSX workbook and return every sheet and cell as a JSON value.
#[napi]
pub fn parse_xlsx_js(path: String, options: Option<ParseOptionsJs>) -> Result<serde_json::Value> {
    let workbook = parse_xlsx(path, to_parse_options(options)).map_err(js_error)?;
    serde_json::to_value(&workbook).map_err(js_error)
}

/// Parse an XLSX workbook and write one NDJSON record per cell to `out_path`.
#[napi]
pub fn parse_xlsx_ndjson_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
) -> Result<()> {
    parse_xlsx_to_ndjson(path, out_path, to_parse_options(options)).map_err(js_error)
}

/// Parse an XLSX workbook and write an Arrow IPC file to `out_path`.
#[napi]
pub fn parse_xlsx_arrow_ipc_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
    arrow_options: Option<ArrowOptionsJs>,
) -> Result<()> {
    parse_xlsx_to_arrow_ipc(
        path,
        out_path,
        to_parse_options(options),
        to_arrow_options(arrow_options),
    )
    .map(|_rows| ())
    .map_err(js_error)
}
