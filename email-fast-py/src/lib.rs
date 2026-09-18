use email_fast_core::arrow::{parse_mbox_to_arrow_ipc, ArrowOptions};
use email_fast_core::{
    extract_attachments, parse_email, parse_email_bytes, parse_email_to_ndjson, parse_eml,
    parse_mbox_to_ndjson, EmailFormat, ParseOptions,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

#[pyfunction]
fn parse_email_json_py(
    path: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<String> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };

    let email =
        parse_email(path, options).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

    serde_json::to_string(&email).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
fn parse_eml_json_py(
    path: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<String> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };

    let email = parse_eml(path, options).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

    serde_json::to_string(&email).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
fn parse_email_ndjson_py(
    path: &str,
    out_path: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<()> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };

    parse_email_to_ndjson(path, out_path, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
fn parse_mbox_ndjson_py(
    path: &str,
    out_path: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<usize> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };

    parse_mbox_to_ndjson(path, out_path, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
// The eight parameters are the released Python signature; collapsing them into
// an options struct would change the wheel surface, not the code quality.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the released Python signature"
)]
fn parse_mbox_arrow_py(
    path: &str,
    out_path: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
    batch_size: Option<usize>,
) -> PyResult<usize> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };

    let arrow_options = ArrowOptions {
        batch_size: batch_size.unwrap_or(10_000),
    };

    parse_mbox_to_arrow_ipc(path, out_path, options, arrow_options)
        .map_err(|err: anyhow::Error| PyRuntimeError::new_err(err.to_string()))
}

#[pyfunction]
fn extract_attachments_py(
    path: &str,
    out_dir: &str,
    max_attachment_size: Option<usize>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<Vec<String>> {
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: false,
        html_to_text: false,
        extract_attachment_content: true, // Always extract content when saving
    };

    let paths = extract_attachments(path, out_dir, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

    Ok(paths
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect())
}

/// Parse an email held in memory, dispatching on an explicit format.
///
/// A caller with an uploaded document must not have to write it to disk first;
/// this is the byte entry point for the `.eml` / `.msg` upload path.
#[pyfunction]
#[pyo3(signature = (source, format, max_attachment_size=None, include_raw_headers=None, html_to_text=None, extract_attachment_content=None, max_total_attachment_bytes=None))]
fn parse_email_bytes_json_py(
    source: &Bound<'_, PyBytes>,
    format: &str,
    max_attachment_size: Option<usize>,
    include_raw_headers: Option<bool>,
    html_to_text: Option<bool>,
    extract_attachment_content: Option<bool>,
    max_total_attachment_bytes: Option<usize>,
) -> PyResult<String> {
    let format = match format {
        "eml" => EmailFormat::Eml,
        "msg" => EmailFormat::Msg,
        other => {
            return Err(PyValueError::new_err(format!(
                "unsupported email byte format: {other}"
            )));
        }
    };
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: max_attachment_size.unwrap_or(100 * 1024 * 1024),
        max_total_attachment_bytes: max_total_attachment_bytes.unwrap_or(100 * 1024 * 1024),
        include_raw_headers: include_raw_headers.unwrap_or(false),
        html_to_text: html_to_text.unwrap_or(false),
        extract_attachment_content: extract_attachment_content.unwrap_or(false),
    };
    let email = parse_email_bytes(source.as_bytes(), format, options)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    serde_json::to_string(&email).map_err(|err| PyRuntimeError::new_err(err.to_string()))
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse_email_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_email_bytes_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_eml_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_email_ndjson_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_mbox_ndjson_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_mbox_arrow_py, m)?)?;
    m.add_function(wrap_pyfunction!(extract_attachments_py, m)?)?;
    Ok(())
}
