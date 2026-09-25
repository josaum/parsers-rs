//! Python bindings for `xlsx-fast-core`.
//!
//! Two entry points, two callers:
//!
//! - `parse_xlsx_bytes_py` — parse an in-memory workbook and return the workbook
//!   JSON as a string. This is the Atlas path: an upload is parsed where it
//!   already is, with no temp file and no subprocess, because the parse is fast
//!   enough to run in-process.
//! - `parse_xlsx_json_py` / `parse_xlsx_ndjson_py` / `parse_xlsx_arrow_ipc_py` —
//!   path-based, writing to a path. These are the Mycelia path: a large workbook
//!   on disk that should stream to NDJSON or Arrow IPC rather than materialize.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;
use xlsx_fast_core::arrow::{parse_xlsx_to_arrow_ipc, ArrowOptions};
use xlsx_fast_core::{parse_xlsx, parse_xlsx_to_ndjson, ParseOptions};

/// Accept anything the caller would call a path.
///
/// Taking `&str` here rejected `pathlib.Path`, which is what most Python
/// callers actually hold — and which the Rust core accepts. A parser that
/// refuses a `Path` is a parser with a needless edge, so the boundary converts
/// with `os.fspath` semantics instead.
fn fs_path(value: &Bound<'_, PyAny>) -> PyResult<PathBuf> {
    if let Ok(text) = value.extract::<String>() {
        return Ok(PathBuf::from(text));
    }
    if let Ok(bytes) = value.extract::<Vec<u8>>() {
        // `os.fspath` permits `bytes` on POSIX; decode what we can and let an
        // undecodable path fail loudly rather than silently mangling it.
        return String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|err| PyRuntimeError::new_err(format!("non-UTF-8 path: {err}")));
    }
    match value.call_method0("__fspath__") {
        Ok(path) => fs_path(&path),
        Err(_) => {
            let kind = value
                .get_type()
                .name()
                .map(|name| name.to_string())
                .unwrap_or_else(|_| "?".to_string());
            Err(PyRuntimeError::new_err(format!(
                "expected a str or os.PathLike path, got {kind}"
            )))
        }
    }
}

fn build_options(
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    raw_values: Option<bool>,
    max_inflate: Option<usize>,
) -> ParseOptions {
    ParseOptions {
        decode_dates: decode_dates.unwrap_or(true),
        include_formulas: include_formulas.unwrap_or(false),
        raw_values: raw_values.unwrap_or(false),
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
    }
}

#[pyfunction]
#[pyo3(signature = (path, decode_dates=None, include_formulas=None, raw_values=None, max_inflate=None))]
fn parse_xlsx_json_py(
    path: &Bound<'_, PyAny>,
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    raw_values: Option<bool>,
    max_inflate: Option<usize>,
) -> PyResult<String> {
    let options = build_options(decode_dates, include_formulas, raw_values, max_inflate);
    let workbook = parse_xlsx(fs_path(path)?, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&workbook).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_path, decode_dates=None, include_formulas=None, raw_values=None, max_inflate=None))]
fn parse_xlsx_ndjson_py(
    path: &Bound<'_, PyAny>,
    out_path: &Bound<'_, PyAny>,
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    raw_values: Option<bool>,
    max_inflate: Option<usize>,
) -> PyResult<()> {
    let options = build_options(decode_dates, include_formulas, raw_values, max_inflate);
    parse_xlsx_to_ndjson(fs_path(path)?, fs_path(out_path)?, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (bytes, decode_dates=None, include_formulas=None, raw_values=None, max_inflate=None))]
fn parse_xlsx_bytes_py(
    bytes: &[u8],
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    raw_values: Option<bool>,
    max_inflate: Option<usize>,
) -> PyResult<String> {
    let options = build_options(decode_dates, include_formulas, raw_values, max_inflate);
    let workbook = xlsx_fast_core::parse_xlsx_bytes(bytes.to_vec(), options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&workbook).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_path, decode_dates=None, include_formulas=None, raw_values=None, max_inflate=None, batch_size=None))]
fn parse_xlsx_arrow_ipc_py(
    path: &Bound<'_, PyAny>,
    out_path: &Bound<'_, PyAny>,
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    raw_values: Option<bool>,
    max_inflate: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<usize> {
    let options = build_options(decode_dates, include_formulas, raw_values, max_inflate);
    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(16_384),
        ..ArrowOptions::default()
    };
    parse_xlsx_to_arrow_ipc(fs_path(path)?, fs_path(out_path)?, options, arrow_options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pymodule]
fn _core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(parse_xlsx_json_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_xlsx_bytes_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_xlsx_ndjson_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_xlsx_arrow_ipc_py, module)?)?;
    Ok(())
}
