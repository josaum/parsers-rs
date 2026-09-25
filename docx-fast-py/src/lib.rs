use docx_fast_core::arrow::{parse_docx_to_arrow_dataset, parse_docx_to_arrow_ipc, ArrowOptions};
use docx_fast_core::{extract_docx_images, parse_docx, parse_docx_to_ndjson, ParseOptions};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[pyfunction]
#[pyo3(signature = (path, max_inflate=None))]
fn parse_docx_json_py(path: &str, max_inflate: Option<usize>) -> PyResult<String> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    let doc = parse_docx(path, options).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&doc).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_path, max_inflate=None))]
fn parse_docx_ndjson_py(path: &str, out_path: &str, max_inflate: Option<usize>) -> PyResult<()> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    parse_docx_to_ndjson(path, out_path, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_path, max_inflate=None, batch_size=None))]
fn parse_docx_arrow_ipc_py(
    path: &str,
    out_path: &str,
    max_inflate: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<()> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(16_384),
    };
    parse_docx_to_arrow_ipc(path, out_path, options, arrow_options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
#[pyo3(signature = (path, out_dir, max_inflate=None, batch_size=None))]
fn parse_docx_arrow_dataset_py(
    path: &str,
    out_dir: &str,
    max_inflate: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<Vec<String>> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(16_384),
    };
    let outputs = parse_docx_to_arrow_dataset(path, out_dir, options, arrow_options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(outputs
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

#[pyfunction]
fn extract_docx_images_py(path: &str, out_dir: &str) -> PyResult<Vec<String>> {
    let images = extract_docx_images(path, out_dir)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(images
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

#[pyfunction]
#[pyo3(signature = (bytes, max_inflate=None))]
fn parse_docx_bytes_py(bytes: &[u8], max_inflate: Option<usize>) -> PyResult<String> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    let doc = docx_fast_core::parse_docx_bytes(bytes.to_vec(), options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&doc).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

/// Parse a DOCX file by path.
///
/// The binding also exposes in-memory parsing; a caller with an upload should
/// prefer that so the document never has to touch the filesystem.
#[pyfunction]
#[pyo3(signature = (path, max_inflate=None))]
fn parse_docx_py(path: &str, max_inflate: Option<usize>) -> PyResult<String> {
    let options = ParseOptions {
        max_inflate: max_inflate.unwrap_or(128 * 1024 * 1024),
        ..ParseOptions::default()
    };
    let document =
        parse_docx(path, options).map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    serde_json::to_string(&document).map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse_docx_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_docx_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_docx_bytes_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_docx_ndjson_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_docx_arrow_ipc_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_docx_arrow_dataset_py, m)?)?;
    m.add_function(wrap_pyfunction!(extract_docx_images_py, m)?)?;
    Ok(())
}
