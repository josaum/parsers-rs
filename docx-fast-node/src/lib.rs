// Rust guideline compliant 2026-02-21
//! Node-API bindings for `docx-fast-core`.
//!
//! Every function takes a file path, mirrors the Python binding one-for-one,
//! and reports parse failures as JavaScript errors. Options objects are
//! partial: an omitted field takes the core crate's default.

use docx_fast_core::arrow::{parse_docx_to_arrow_dataset, parse_docx_to_arrow_ipc, ArrowOptions};
use docx_fast_core::{extract_docx_images, parse_docx, parse_docx_to_ndjson, ParseOptions};
use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Parse options exposed to Node.js consumers.
#[napi(object)]
pub struct ParseOptionsJs {
    /// Aggregate inflate budget for the archive, in bytes (default: 128 MiB).
    pub max_inflate: Option<u32>,
}

/// Arrow output options exposed to Node.js consumers.
#[napi(object)]
pub struct ArrowOptionsJs {
    /// Rows per Arrow record batch (default: the core crate's).
    pub batch_size: Option<u32>,
}

/// Build [`ParseOptions`] from a partial JS object, filling unset fields from the core default.
fn to_parse_options(opts: Option<ParseOptionsJs>) -> ParseOptions {
    let d = ParseOptions::default();
    ParseOptions {
        max_inflate: opts
            .and_then(|o| o.max_inflate)
            .map_or(d.max_inflate, |v| v as usize),
        ..d
    }
}

/// Build [`ArrowOptions`] from a partial JS object, filling unset fields from the core default.
fn to_arrow_options(opts: Option<ArrowOptionsJs>) -> ArrowOptions {
    let d = ArrowOptions::default();
    ArrowOptions {
        batch_size: opts
            .and_then(|o| o.batch_size)
            .map_or(d.batch_size, |v| v as usize),
    }
}

/// Convert a core error into a JavaScript error carrying its message.
fn js_error(err: impl std::fmt::Display) -> Error {
    Error::from_reason(err.to_string())
}

/// Parse a DOCX file and return the whole document as a JSON value.
#[napi]
pub fn parse_docx_js(path: String, options: Option<ParseOptionsJs>) -> Result<serde_json::Value> {
    let doc = parse_docx(path, to_parse_options(options)).map_err(js_error)?;
    serde_json::to_value(&doc).map_err(js_error)
}

/// Parse a DOCX file and write one NDJSON record per block to `out_path`.
#[napi]
pub fn parse_docx_ndjson_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
) -> Result<()> {
    parse_docx_to_ndjson(path, out_path, to_parse_options(options)).map_err(js_error)
}

/// Extract every embedded image into `out_dir` and return the written paths.
#[napi]
pub fn extract_docx_images_js(path: String, out_dir: String) -> Result<Vec<String>> {
    let images = extract_docx_images(path, out_dir).map_err(js_error)?;
    Ok(images
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}

/// Parse a DOCX file and write its Arrow IPC tables under `out_dir`.
#[napi]
pub fn parse_docx_arrow_ipc_js(
    path: String,
    out_dir: String,
    options: Option<ParseOptionsJs>,
    arrow_options: Option<ArrowOptionsJs>,
) -> Result<()> {
    parse_docx_to_arrow_ipc(
        path,
        out_dir,
        to_parse_options(options),
        to_arrow_options(arrow_options),
    )
    .map_err(js_error)
}

/// Parse a DOCX file into an Arrow dataset under `out_dir` and return the written files.
#[napi]
pub fn parse_docx_arrow_dataset_js(
    path: String,
    out_dir: String,
    options: Option<ParseOptionsJs>,
    arrow_options: Option<ArrowOptionsJs>,
) -> Result<Vec<String>> {
    let outputs = parse_docx_to_arrow_dataset(
        path,
        out_dir,
        to_parse_options(options),
        to_arrow_options(arrow_options),
    )
    .map_err(js_error)?;
    Ok(outputs
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}
