use pdf_fast_write::PdfWriter;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Extract specific 1-based pages from `source` into a new PDF at `output`.
#[pyfunction]
fn extract_pdf_pages_py(source: &str, pages: Vec<usize>, output: &str) -> PyResult<()> {
    PdfWriter::extract_pages(source, &pages, output)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// Extract specific 1-based pages from a PDF path and return subset PDF bytes.
#[pyfunction]
fn extract_pdf_pages_to_bytes_py(source: &str, pages: Vec<usize>) -> PyResult<Vec<u8>> {
    PdfWriter::extract_pages_to_bytes(source, &pages)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// Extract specific 1-based pages from in-memory PDF bytes.
#[pyfunction]
fn extract_pdf_pages_from_bytes_py(data: &[u8], pages: Vec<usize>) -> PyResult<Vec<u8>> {
    PdfWriter::extract_pages_from_bytes(data, &pages)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(extract_pdf_pages_py, m)?)?;
    m.add_function(wrap_pyfunction!(extract_pdf_pages_to_bytes_py, m)?)?;
    m.add_function(wrap_pyfunction!(extract_pdf_pages_from_bytes_py, m)?)?;
    Ok(())
}
