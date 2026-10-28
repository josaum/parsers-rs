//! Bytes-only Python binding for the canonical RDF/Turtle ontology reader.
//!
//! Four functions, each a thin projection of `ontology-fast-core`:
//! `parse_triples` for the frozen text contract, `parse_terms` for the lossless
//! term view the app's MCP ontology surfaces rebuild their graphs from, `query`
//! for the indexed `(subject, predicate, object)` lookup the routing core
//! performs, and `prefixes` for the document's own declarations. Nothing here
//! promotes assurance: parsing preserves structure, and the rows are unsigned.
//!
//! Every parse runs with the GIL released: the input is an immutable `bytes`
//! object borrowed for the call, and the core touches no Python state, so other
//! Python threads keep running while a large module is read.
//!
//! Refusals raise `OntologyContractError` (a `ValueError`) or one of its two
//! subclasses, chosen from the core's stable error code, never from the message.
//! All pure logic — limits, pattern selection, the code itself — lives in
//! `ontology-fast-core`, where a Rust test can reach it; this crate links as a
//! Python extension and holds only the conversion to and from Python objects.

use ontology_fast_core::{
    declared_prefixes, parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits, TermView,
};
use pyo3::{
    create_exception,
    exceptions::PyValueError,
    prelude::*,
    types::{PyBytes, PyDict, PyList, PyTuple},
};

create_exception!(
    ontology_fast,
    OntologyContractError,
    PyValueError,
    "The reader refused the document. Subclasses name a malformed document and \
     an exceeded budget; raised as itself, a term cannot be rendered without \
     inventing structure the document lacks."
);
create_exception!(
    ontology_fast,
    OntologySyntaxError,
    OntologyContractError,
    "The bytes are not a valid UTF-8 Turtle document."
);
create_exception!(
    ontology_fast,
    OntologyResourceError,
    OntologyContractError,
    "The document exceeded a source-byte or statement budget."
);

/// Raise the exception type the core's stable code names, carrying that code.
fn ontology_error(py: Python<'_>, error: &OntologyError) -> PyErr {
    let message = error.to_string();
    let raised = match error {
        OntologyError::Syntax(_) => OntologySyntaxError::new_err(message),
        OntologyError::ResourceLimit { .. } => OntologyResourceError::new_err(message),
        OntologyError::TripleTerm { .. }
        | OntologyError::BaseDirection { .. }
        | OntologyError::NamedGraph(_) => OntologyContractError::new_err(message),
    };
    // Best effort: an exception that cannot take the attribute is still the
    // right exception, and failing here would mask the refusal being reported.
    let _ = raised.value(py).setattr("code", error.code());
    raised
}

/// One `parse_terms` row: `(subject, predicate, object, kind, datatype, language)`.
type TermRow<'a> = (
    &'a str,
    &'a str,
    &'a str,
    &'static str,
    Option<&'a str>,
    Option<&'a str>,
);

/// Return every triple as a `(subject, predicate, object)` text tuple.
///
/// Terms render through the document's own `@prefix` declarations, `rdf:type`
/// renders as `a`, and a literal renders as its lexical form. A blank node
/// renders as a document-scoped `_:label`, identical on every parse of the same
/// bytes. RDF 1.2 triple terms raise `OntologyContractError` rather than being
/// rendered into an artefact of this implementation.
#[pyfunction]
#[pyo3(signature = (source, *, max_source_bytes=None, max_statements=None))]
fn parse_triples(
    py: Python<'_>,
    source: &Bound<'_, PyBytes>,
    max_source_bytes: Option<usize>,
    max_statements: Option<usize>,
) -> PyResult<Py<PyList>> {
    let limits = OntologyLimits::with_overrides(max_source_bytes, max_statements);
    let bytes = source.as_bytes();
    let ontology = py
        .detach(|| parse_turtle(bytes, limits))
        .map_err(|error| ontology_error(py, &error))?;

    let rows: Vec<Py<PyTuple>> = ontology
        .triples()
        .iter()
        .map(|triple| {
            PyTuple::new(
                py,
                [
                    triple.subject.as_str(),
                    triple.predicate.as_str(),
                    triple.object.as_str(),
                ],
            )
            .map(Bound::unbind)
        })
        .collect::<PyResult<_>>()?;
    Ok(PyList::new(py, rows)?.unbind())
}

/// Return every triple in the lossless term view.
///
/// Each row is a `(subject, predicate, object, kind, datatype, language)`
/// tuple. IRIs are full, never abbreviated, and `rdf:type` stays a full IRI.
/// `kind` is `"iri"`, `"blank"`, or `"literal"`; a blank node renders a
/// document-scoped `_:label`, identical on every parse of the same bytes. For a literal, `datatype` is the
/// datatype IRI of a typed literal and `language` the lowercased language tag;
/// both are `None` for a simple literal and for IRI or blank-node objects.
///
/// A literal written `"..."^^xsd:string` reads as a simple literal: RDF 1.2
/// Concepts §3.3 makes it the same term, while rdflib keeps the datatype. That
/// is a recorded contract difference, not a parse failure. Literals carrying
/// an RDF 1.2 base direction, RDF 1.2 triple terms, and named graphs are
/// refused with `OntologyContractError`.
#[pyfunction]
#[pyo3(signature = (source, *, max_source_bytes=None, max_statements=None))]
fn parse_terms(
    py: Python<'_>,
    source: &Bound<'_, PyBytes>,
    max_source_bytes: Option<usize>,
    max_statements: Option<usize>,
) -> PyResult<Py<PyList>> {
    let limits = OntologyLimits::with_overrides(max_source_bytes, max_statements);
    let bytes = source.as_bytes();
    let triples = py
        .detach(|| parse_turtle_terms(bytes, limits))
        .map_err(|error| ontology_error(py, &error))?;

    let rows: Vec<TermRow<'_>> = triples
        .iter()
        .map(|triple| {
            let (object, kind, datatype, language) = match &triple.object {
                TermView::Iri(iri) => (iri.as_str(), "iri", None, None),
                TermView::Blank(label) => (label.as_str(), "blank", None, None),
                TermView::Literal {
                    value,
                    datatype,
                    language,
                } => (
                    value.as_str(),
                    "literal",
                    datatype.as_deref(),
                    language.as_deref(),
                ),
            };
            (
                triple.subject.as_str(),
                triple.predicate.as_str(),
                object,
                kind,
                datatype,
                language,
            )
        })
        .collect();
    Ok(PyList::new(py, rows)?.unbind())
}

/// Return the document's own prefix declarations as `{prefix: namespace}`.
///
/// The table is the Turtle parser's, so `@prefix` and SPARQL-style `PREFIX`
/// both declare and a directive-looking line inside a literal does not. A
/// prefix bound twice reports its final binding. A document that does not
/// parse, or exceeds `max_source_bytes`, is refused exactly as `parse_triples`
/// refuses it.
#[pyfunction]
#[pyo3(signature = (source, *, max_source_bytes=None))]
fn prefixes(
    py: Python<'_>,
    source: &Bound<'_, PyBytes>,
    max_source_bytes: Option<usize>,
) -> PyResult<Py<PyDict>> {
    let limits = OntologyLimits::with_overrides(max_source_bytes, None);
    let bytes = source.as_bytes();
    let declared = py
        .detach(|| declared_prefixes(bytes, limits))
        .map_err(|error| ontology_error(py, &error))?;
    let dict = PyDict::new(py);
    for (prefix, namespace) in declared {
        dict.set_item(prefix, namespace)?;
    }
    Ok(dict.unbind())
}

/// Answer the routing core's `(s, p, o)` patterns over one parsed document.
///
/// Each pattern is a 3-tuple whose entries are a term string or `None` for a
/// wildcard. A pattern with no bound position is refused: matching everything
/// is a scan, not a lookup, and callers that want every triple should ask for
/// them once instead of per pattern.
#[pyfunction]
#[pyo3(signature = (source, patterns, *, max_source_bytes=None, max_statements=None))]
fn query(
    py: Python<'_>,
    source: &Bound<'_, PyBytes>,
    patterns: &Bound<'_, PyList>,
    max_source_bytes: Option<usize>,
    max_statements: Option<usize>,
) -> PyResult<Py<PyList>> {
    let limits = OntologyLimits::with_overrides(max_source_bytes, max_statements);
    let bytes = source.as_bytes();
    let ontology = py
        .detach(|| parse_turtle(bytes, limits))
        .map_err(|error| ontology_error(py, &error))?;
    let index = ontology.index();
    let triples = ontology.triples();

    let mut rows: Vec<Py<PyTuple>> = Vec::new();
    for pattern in patterns.iter() {
        let pattern = pattern.cast::<PyTuple>()?;
        if pattern.len() != 3 {
            return Err(PyValueError::new_err(
                "each pattern must be a (subject, predicate, object) tuple",
            ));
        }
        let subject: Option<String> = pattern.get_item(0)?.extract()?;
        let predicate: Option<String> = pattern.get_item(1)?.extract()?;
        let object: Option<String> = pattern.get_item(2)?.extract()?;
        if subject.is_none() && predicate.is_none() && object.is_none() {
            return Err(PyValueError::new_err(
                "a pattern must bind at least one position",
            ));
        }
        for ordinal in index.select(subject.as_deref(), predicate.as_deref(), object.as_deref()) {
            let triple = &triples[ordinal as usize];
            rows.push(
                PyTuple::new(
                    py,
                    [
                        triple.subject.as_str(),
                        triple.predicate.as_str(),
                        triple.object.as_str(),
                    ],
                )?
                .unbind(),
            );
        }
    }
    Ok(PyList::new(py, rows)?.unbind())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    let py = module.py();
    module.add(
        "OntologyContractError",
        py.get_type::<OntologyContractError>(),
    )?;
    module.add("OntologySyntaxError", py.get_type::<OntologySyntaxError>())?;
    module.add(
        "OntologyResourceError",
        py.get_type::<OntologyResourceError>(),
    )?;
    module.add_function(wrap_pyfunction!(parse_triples, module)?)?;
    module.add_function(wrap_pyfunction!(parse_terms, module)?)?;
    module.add_function(wrap_pyfunction!(prefixes, module)?)?;
    module.add_function(wrap_pyfunction!(query, module)?)?;
    Ok(())
}
