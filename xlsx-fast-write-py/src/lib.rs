use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use xlsx_fast_write::Workbook;

/// Write an XLSX file from a JSON workbook spec to `out_path`.
#[pyfunction]
fn write_xlsx_json_py(json: &str, out_path: &str) -> PyResult<()> {
    Workbook::write_json_spec(json, out_path).map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// Write an XLSX file from a JSON workbook spec and return the bytes.
#[pyfunction]
fn write_xlsx_json_bytes_py(py: Python<'_>, json: &str) -> PyResult<Py<PyBytes>> {
    let bytes = Workbook::write_json_spec_to_bytes(json)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    Ok(PyBytes::new(py, &bytes).into())
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(write_xlsx_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(write_xlsx_json_bytes_py, m)?)?;
    Ok(())
}
