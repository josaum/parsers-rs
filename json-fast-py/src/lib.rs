//! Thin PyO3 bindings for `json-fast-core`.
//!
//! Surfaces the SIMD JSON/JSONL parse path and the JSON-family helpers to the
//! Python control plane. `loads` returns real Python objects (list/dict/scalar)
//! — no JSON-string roundtrip — and `loads_many` parses a JSONL batch. The
//! helpers `json_type`, `validate`, `jsonrpc_*`, `jsonapi_document`,
//! `jwt_parts` and `jsonld_meta` cover the six JSON types and the derived
//! formats.

#![deny(missing_docs)]

use json_fast_core::{jsonl, JsonError, JsonLimits};
use pyo3::exceptions::PyRuntimeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyList, PyModule};
use serde_json::Value;

pyo3::create_exception!(
    json_fast,
    JsonResourceError,
    PyRuntimeError,
    "Raised when configured JSON or JSONL resource limits are exceeded."
);

fn err(error: JsonError) -> PyErr {
    if matches!(&error, JsonError::ResourceLimit { .. }) {
        JsonResourceError::new_err(error.to_string())
    } else {
        PyRuntimeError::new_err(error.to_string())
    }
}

fn runtime_err(error: impl ToString) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn configured_limits(
    max_input_bytes: Option<usize>,
    max_nesting_depth: Option<usize>,
    max_tokens: Option<usize>,
    max_lines: Option<usize>,
    max_records: Option<usize>,
    max_retained_bytes: Option<usize>,
) -> JsonLimits {
    let mut limits = JsonLimits::default();
    if let Some(value) = max_input_bytes {
        limits.max_input_bytes = value;
    }
    if let Some(value) = max_nesting_depth {
        limits.max_nesting_depth = value;
    }
    if let Some(value) = max_tokens {
        limits.max_tokens = value;
    }
    if let Some(value) = max_lines {
        limits.max_lines = value;
    }
    if let Some(value) = max_records {
        limits.max_records = value;
    }
    if let Some(value) = max_retained_bytes {
        limits.max_retained_bytes = value;
    }
    limits
}

fn value_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.to_owned().into_any())
            } else if let Some(u) = n.as_u64() {
                Ok(u.into_pyobject(py)?.to_owned().into_any())
            } else {
                let float = n.as_f64().unwrap_or(f64::NAN);
                Ok(float.into_pyobject(py)?.to_owned().into_any())
            }
        }
        Value::String(s) => Ok(s.as_str().into_pyobject(py)?.to_owned().into_any()),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_to_py(py, item)?)?;
            }
            Ok(list.into_any())
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

/// Crate / binding version string.
#[pyfunction]
fn version() -> &'static str {
    json_fast_core::VERSION
}

/// Parse a JSON document into Python objects (SIMD fast path).
///
/// Returns `dict` / `list` / `str` / `int` / `float` / `bool` / `None`,
/// matching the six JSON types.
#[pyfunction]
#[pyo3(signature = (s, *, max_input_bytes=None, max_nesting_depth=None, max_tokens=None, max_retained_bytes=None))]
fn loads<'py>(
    py: Python<'py>,
    s: &str,
    max_input_bytes: Option<usize>,
    max_nesting_depth: Option<usize>,
    max_tokens: Option<usize>,
    max_retained_bytes: Option<usize>,
) -> PyResult<Bound<'py, PyAny>> {
    let limits = configured_limits(
        max_input_bytes,
        max_nesting_depth,
        max_tokens,
        None,
        None,
        max_retained_bytes,
    );
    let value: Value = json_fast_core::parse_with_limits(s, &limits).map_err(err)?;
    value_to_py(py, &value)
}

/// Parse a JSONL document into a Python list of objects (blank lines skipped).
// Independent keyword budgets are the public Python binding contract.
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (jsonl_text, *, max_input_bytes=None, max_nesting_depth=None, max_tokens=None, max_lines=None, max_records=None, max_retained_bytes=None))]
fn loads_many<'py>(
    py: Python<'py>,
    jsonl_text: &str,
    max_input_bytes: Option<usize>,
    max_nesting_depth: Option<usize>,
    max_tokens: Option<usize>,
    max_lines: Option<usize>,
    max_records: Option<usize>,
    max_retained_bytes: Option<usize>,
) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let limits = configured_limits(
        max_input_bytes,
        max_nesting_depth,
        max_tokens,
        max_lines,
        max_records,
        max_retained_bytes,
    );
    let values: Vec<Value> = jsonl::parse_many_with_limits(jsonl_text, &limits).map_err(err)?;
    values.iter().map(|v| value_to_py(py, v)).collect()
}

/// Count non-blank lines in a JSONL document (fast streaming pass).
#[pyfunction]
fn count_lines(jsonl_text: &str) -> PyResult<usize> {
    Ok(jsonl::count_lines(jsonl_text))
}

/// Return the six-type classification of a JSON document's root value.
#[pyfunction]
fn json_type(s: &str) -> PyResult<String> {
    let value: Value = json_fast_core::parse(s).map_err(err)?;
    Ok(json_fast_core::value_type(&value).as_str().to_string())
}

/// Validate a JSON document against a JSON Schema; returns a list of findings.
///
/// Each finding is `{"severity": "error"|"warning", "path", "message"}`.
#[pyfunction]
fn validate<'py>(py: Python<'py>, s: &str, schema: &str) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let doc: Value = json_fast_core::parse(s).map_err(err)?;
    let sch: Value = json_fast_core::parse(schema).map_err(err)?;
    json_fast_core::schema::validate(&doc, &sch)
        .into_iter()
        .map(|f| {
            let d = PyDict::new(py);
            d.set_item(intern!(py, "severity"), f.severity.as_str())?;
            d.set_item(intern!(py, "path"), &f.path)?;
            d.set_item(intern!(py, "message"), &f.message)?;
            Ok(d.into_any())
        })
        .collect()
}

/// Parse and validate a JSON-RPC 2.0 request envelope → dict.
#[pyfunction]
fn jsonrpc_request<'py>(py: Python<'py>, s: &str) -> PyResult<Bound<'py, PyAny>> {
    let request = json_fast_core::jsonrpc::JsonRpcRequest::parse(s).map_err(err)?;
    let value: Value = serde_json::to_value(&request).map_err(runtime_err)?;
    value_to_py(py, &value)
}

/// Parse and validate a JSON-RPC 2.0 response envelope → dict.
#[pyfunction]
fn jsonrpc_response<'py>(py: Python<'py>, s: &str) -> PyResult<Bound<'py, PyAny>> {
    let response = json_fast_core::jsonrpc::JsonRpcResponse::parse(s).map_err(err)?;
    let value: Value = serde_json::to_value(&response).map_err(runtime_err)?;
    value_to_py(py, &value)
}

/// Parse and validate a JSON:API document envelope → dict.
#[pyfunction]
fn jsonapi_document<'py>(py: Python<'py>, s: &str) -> PyResult<Bound<'py, PyAny>> {
    let document = json_fast_core::jsonapi::JsonApiDocument::parse(s).map_err(err)?;
    let value: Value = serde_json::to_value(&document).map_err(runtime_err)?;
    value_to_py(py, &value)
}

/// Split a compact JWT into `{"header", "payload", "signature"}`.
#[pyfunction]
fn jwt_parts<'py>(py: Python<'py>, token: &str) -> PyResult<Bound<'py, PyAny>> {
    let parts = json_fast_core::jwt::split_jwt(token).map_err(err)?;
    let d = PyDict::new(py);
    d.set_item(intern!(py, "header"), value_to_py(py, &parts.header)?)?;
    d.set_item(intern!(py, "payload"), value_to_py(py, &parts.payload)?)?;
    d.set_item(intern!(py, "signature"), parts.signature)?;
    Ok(d.into_any())
}

/// Peek at a JSON-LD document's `@context` / `@graph` / `@type` → dict.
#[pyfunction]
fn jsonld_meta<'py>(py: Python<'py>, s: &str) -> PyResult<Bound<'py, PyAny>> {
    let meta = json_fast_core::jsonld::peek(s).map_err(err)?;
    let d = PyDict::new(py);
    if let Some(ctx) = &meta.context {
        d.set_item(intern!(py, "context"), value_to_py(py, ctx)?)?;
    }
    if let Some(g) = &meta.graph {
        d.set_item(intern!(py, "graph"), value_to_py(py, g)?)?;
    }
    if let Some(t) = &meta.type_ {
        d.set_item(intern!(py, "type"), value_to_py(py, t)?)?;
    }
    Ok(d.into_any())
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("JsonResourceError", m.py().get_type::<JsonResourceError>())?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(loads, m)?)?;
    m.add_function(wrap_pyfunction!(loads_many, m)?)?;
    m.add_function(wrap_pyfunction!(count_lines, m)?)?;
    m.add_function(wrap_pyfunction!(json_type, m)?)?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    m.add_function(wrap_pyfunction!(jsonrpc_request, m)?)?;
    m.add_function(wrap_pyfunction!(jsonrpc_response, m)?)?;
    m.add_function(wrap_pyfunction!(jsonapi_document, m)?)?;
    m.add_function(wrap_pyfunction!(jwt_parts, m)?)?;
    m.add_function(wrap_pyfunction!(jsonld_meta, m)?)?;
    Ok(())
}
