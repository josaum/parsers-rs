//! Python bindings for xsd-fast-core.
//!
//! Exposed as the `xsd_fast._core` native extension module.
//!
//! ## Python usage
//!
//! ```python
//! from xsd_fast import _core
//!
//! # Parse a schema and list its global elements
//! elements = _core.list_elements_py("/path/to/schema.xsd")
//!
//! # Get Arrow schema description as JSON
//! desc = _core.describe_element_json_py("/path/to/schema.xsd", "example")
//! # desc is a JSON string: [{"path": "...", "arrow_type": "...", "nullable": "..."}]
//!
//! # Validate an XML file; returns list of {"severity","path","message"} dicts
//! errors = _core.validate_json_py("/path/to/schema.xsd", "/path/to/doc.xml")
//!
//! # Validate an XML bytes object directly
//! errors = _core.validate_bytes_json_py("/path/to/schema.xsd", xml_bytes)
//!
//! # Write Arrow IPC schema description to a file
//! _core.schema_ipc_bytes_py("/path/to/schema.xsd", "example")
//! ```

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

fn map_err(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// List all global element names in the schema.
///
/// Follows `xs:include` / `xs:import` relative paths automatically.
#[pyfunction]
#[pyo3(signature = (schema_path, max_includes=None))]
fn list_elements_py(schema_path: &str, max_includes: Option<usize>) -> PyResult<Vec<String>> {
    let opts = ParseOptions {
        max_includes: max_includes.unwrap_or(64),
        ..Default::default()
    };
    let schema = SchemaSet::load(schema_path, opts).map_err(map_err)?;
    Ok(sorted_elements(&schema))
}

/// List all global element names in an in-memory schema.
#[pyfunction]
#[pyo3(signature = (schema_bytes, max_includes=None))]
fn list_elements_bytes_py(
    schema_bytes: &Bound<'_, PyBytes>,
    max_includes: Option<usize>,
) -> PyResult<Vec<String>> {
    let opts = ParseOptions {
        max_includes: max_includes.unwrap_or(64),
        ..Default::default()
    };
    let schema = SchemaSet::load_bytes(schema_bytes.as_bytes(), opts).map_err(map_err)?;
    Ok(sorted_elements(&schema))
}

fn sorted_elements(schema: &SchemaSet) -> Vec<String> {
    let mut elements = schema
        .model()
        .global_elements
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    elements.sort_unstable();
    elements
}

/// Return Arrow schema description for a root element as a JSON string.
///
/// The JSON is an array of objects: `[{"path": "...", "arrow_type": "...", "nullable": "..."}]`.
#[pyfunction]
#[pyo3(signature = (schema_path, element, max_depth=None, map_dates=None, include_attributes=None))]
fn describe_element_json_py(
    schema_path: &str,
    element: &str,
    max_depth: Option<usize>,
    map_dates: Option<bool>,
    include_attributes: Option<bool>,
) -> PyResult<String> {
    let parse_opts = ParseOptions::default();
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8),
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: include_attributes.unwrap_or(true),
        batch_size: 8_192,
    };
    let schema = SchemaSet::load(schema_path, parse_opts).map_err(map_err)?;
    describe_schema_json(&schema, element, arrow_opts)
}

/// Return Arrow schema description for an in-memory root schema as JSON.
#[pyfunction]
#[pyo3(signature = (schema_bytes, element, max_depth=None, map_dates=None, include_attributes=None))]
fn describe_element_bytes_json_py(
    schema_bytes: &Bound<'_, PyBytes>,
    element: &str,
    max_depth: Option<usize>,
    map_dates: Option<bool>,
    include_attributes: Option<bool>,
) -> PyResult<String> {
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8),
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: include_attributes.unwrap_or(true),
        batch_size: 8_192,
    };
    let schema =
        SchemaSet::load_bytes(schema_bytes.as_bytes(), ParseOptions::default()).map_err(map_err)?;
    describe_schema_json(&schema, element, arrow_opts)
}

fn describe_schema_json(
    schema: &SchemaSet,
    element: &str,
    arrow_options: ArrowOptions,
) -> PyResult<String> {
    let arrow_schema = schema
        .to_arrow_schema(element, arrow_options)
        .map_err(map_err)?;

    let fields: Vec<serde_json::Value> = arrow_schema
        .fields()
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.name(),
                "arrow_type": format!("{:?}", f.data_type()),
                "nullable": f.is_nullable(),
            })
        })
        .collect();

    serde_json::to_string(&fields).map_err(map_err)
}

/// Validate an XML file against the schema.
///
/// Returns a JSON string: `[{"severity": "Error"|"Warning", "path": "...", "message": "..."}]`.
#[pyfunction]
#[pyo3(signature = (schema_path, xml_path, max_errors=None, check_facets=None, check_cardinality=None))]
fn validate_json_py(
    schema_path: &str,
    xml_path: &str,
    max_errors: Option<usize>,
    check_facets: Option<bool>,
    check_cardinality: Option<bool>,
) -> PyResult<String> {
    let parse_opts = ParseOptions::default();
    let validate_opts = ValidateOptions {
        max_errors: max_errors.unwrap_or(100),
        check_facets: check_facets.unwrap_or(true),
        check_cardinality: check_cardinality.unwrap_or(true),
    };
    let xml = std::fs::read(xml_path).map_err(map_err)?;
    let schema = SchemaSet::load(schema_path, parse_opts).map_err(map_err)?;
    let errors = schema.validate(&xml, validate_opts).map_err(map_err)?;
    serde_json::to_string(&errors).map_err(map_err)
}

/// Validate an XML bytes object against the schema.
///
/// Returns a JSON string: `[{"severity": "Error"|"Warning", "path": "...", "message": "..."}]`.
#[pyfunction]
#[pyo3(signature = (schema_path, xml_bytes, max_errors=None, check_facets=None, check_cardinality=None))]
fn validate_bytes_json_py<'py>(
    schema_path: &str,
    xml_bytes: &Bound<'py, PyBytes>,
    max_errors: Option<usize>,
    check_facets: Option<bool>,
    check_cardinality: Option<bool>,
) -> PyResult<String> {
    let parse_opts = ParseOptions::default();
    let validate_opts = ValidateOptions {
        max_errors: max_errors.unwrap_or(100),
        check_facets: check_facets.unwrap_or(true),
        check_cardinality: check_cardinality.unwrap_or(true),
    };
    let schema = SchemaSet::load(schema_path, parse_opts).map_err(map_err)?;
    let errors = schema
        .validate(xml_bytes.as_bytes(), validate_opts)
        .map_err(map_err)?;
    serde_json::to_string(&errors).map_err(map_err)
}

/// Return Arrow IPC schema description bytes for a root element.
#[pyfunction]
#[pyo3(signature = (schema_path, element, max_depth=None, map_dates=None))]
fn schema_ipc_bytes_py(
    py: Python<'_>,
    schema_path: &str,
    element: &str,
    max_depth: Option<usize>,
    map_dates: Option<bool>,
) -> PyResult<Py<PyBytes>> {
    let parse_opts = ParseOptions::default();
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8),
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: true,
        batch_size: 8_192,
    };
    let schema = SchemaSet::load(schema_path, parse_opts).map_err(map_err)?;
    let mut buf = Vec::new();
    schema
        .write_schema_ipc(element, arrow_opts, &mut buf)
        .map_err(map_err)?;
    Ok(PyBytes::new(py, &buf).unbind())
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(list_elements_py, m)?)?;
    m.add_function(wrap_pyfunction!(list_elements_bytes_py, m)?)?;
    m.add_function(wrap_pyfunction!(describe_element_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(describe_element_bytes_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(validate_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(validate_bytes_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(schema_ipc_bytes_py, m)?)?;
    Ok(())
}
