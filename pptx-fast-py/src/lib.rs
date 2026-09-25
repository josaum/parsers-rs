//! Python bindings for `pptx-fast-core`.
//!
//! Two entry points, two callers:
//!
//! - `parse_pptx_bytes_py` — parse an in-memory presentation and return the
//!   document JSON as a string. An upload is parsed where it already is, with
//!   no temp file and no subprocess.
//! - the path-based functions — parse a presentation already on disk and
//!   either return its JSON or stream it to NDJSON, Arrow IPC, or a directory
//!   of Arrow tables.

use pptx_fast_core::arrow::{
    parse_pptx_to_arrow_dataset, parse_pptx_to_arrow_ipc_bytes, ArrowOptions,
};
use pptx_fast_core::{extract_pptx_images, parse_pptx, parse_pptx_to_ndjson, ParseOptions};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;

/// Accept anything the caller would call a path.
///
/// Taking `&str` here rejects `pathlib.Path`, which is what most Python callers
/// actually hold — and which the Rust core accepts. A parser that refuses a
/// `Path` is a parser with a needless edge, so the boundary converts with
/// `os.fspath` semantics instead.
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

fn build_options(max_inflate: Option<usize>) -> ParseOptions {
    ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    }
}

#[pyfunction]
#[pyo3(signature = (path, max_inflate=None))]
fn parse_pptx_json_py(path: &Bound<'_, PyAny>, max_inflate: Option<usize>) -> PyResult<String> {
    let doc = parse_pptx(fs_path(path)?, build_options(max_inflate))
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&doc).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_path, max_inflate=None))]
fn parse_pptx_ndjson_py(
    path: &Bound<'_, PyAny>,
    out_path: &Bound<'_, PyAny>,
    max_inflate: Option<usize>,
) -> PyResult<()> {
    parse_pptx_to_ndjson(
        fs_path(path)?,
        fs_path(out_path)?,
        build_options(max_inflate),
    )
    .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

/// Encode one named shredded table to `out_path` as an in-memory-built Arrow
/// IPC stream; return the number of bytes written.
#[pyfunction]
#[pyo3(signature = (path, out_path, table, max_inflate=None, batch_size=None))]
fn parse_pptx_arrow_ipc_bytes_py(
    path: &Bound<'_, PyAny>,
    out_path: &Bound<'_, PyAny>,
    table: &str,
    max_inflate: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<usize> {
    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(16_384),
    };
    let ipc = parse_pptx_to_arrow_ipc_bytes(
        fs_path(path)?,
        build_options(max_inflate),
        arrow_options,
        table,
    )
    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    std::fs::write(fs_path(out_path)?, &ipc)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(ipc.len())
}

/// Write every shredded table into `out_dir`; return the paths written.
#[pyfunction]
#[pyo3(signature = (path, out_dir, max_inflate=None, batch_size=None))]
fn parse_pptx_arrow_dataset_py(
    path: &Bound<'_, PyAny>,
    out_dir: &Bound<'_, PyAny>,
    max_inflate: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<Vec<String>> {
    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(16_384),
    };
    let outputs = parse_pptx_to_arrow_dataset(
        fs_path(path)?,
        fs_path(out_dir)?,
        build_options(max_inflate),
        arrow_options,
    )
    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(outputs
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

/// Write every embedded image into `out_dir`; return the paths written.
///
/// `max_inflate` is accepted here for the same reason the parse takes it: an
/// extraction that used its own constant would be a way around the caller's
/// budget.
#[pyfunction]
#[pyo3(signature = (path, out_dir, max_inflate=None))]
fn extract_pptx_images_py(
    path: &Bound<'_, PyAny>,
    out_dir: &Bound<'_, PyAny>,
    max_inflate: Option<usize>,
) -> PyResult<Vec<String>> {
    let images = extract_pptx_images(
        fs_path(path)?,
        fs_path(out_dir)?,
        build_options(max_inflate),
    )
    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(images
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

#[pyfunction]
#[pyo3(signature = (bytes, max_inflate=None))]
fn parse_pptx_bytes_py(bytes: &[u8], max_inflate: Option<usize>) -> PyResult<String> {
    let doc = pptx_fast_core::parse_pptx_bytes(bytes.to_vec(), build_options(max_inflate))
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&doc).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pymodule]
fn _core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(parse_pptx_json_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_pptx_bytes_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_pptx_ndjson_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_pptx_arrow_ipc_bytes_py, module)?)?;
    module.add_function(wrap_pyfunction!(parse_pptx_arrow_dataset_py, module)?)?;
    module.add_function(wrap_pyfunction!(extract_pptx_images_py, module)?)?;
    Ok(())
}
