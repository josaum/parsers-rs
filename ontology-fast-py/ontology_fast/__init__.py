"""Bytes-only access to Atlas's canonical ontology reader.

The routing core reads ``app/cartridge/ontology/atlas.ttl`` through
``catalog._subject_blocks``, a regular expression over one authoring style. This
module is the real Turtle reader the ontology-core migration plan moves to, and
it renders terms in exactly the text form the Python core already consumes so
the change can be measured against the frozen contract in
``app/scripts/ontology_triple_contract.py`` rather than argued about.
"""

from __future__ import annotations

from ontology_fast import _native

OntologyContractError = _native.OntologyContractError
OntologySyntaxError = _native.OntologySyntaxError
OntologyResourceError = _native.OntologyResourceError


def parse_triples(
    source: bytes,
    *,
    max_source_bytes: int | None = None,
    max_statements: int | None = None,
) -> list[tuple[str, str, str]]:
    """Return every triple as a ``(subject, predicate, object)`` text tuple.

    Terms render through the document's own prefix declarations (``@prefix``
    or SPARQL-style ``PREFIX``, as the Turtle parser reports them). A blank node
    renders as a document-scoped ``_:label`` that is identical on every parse of
    the same bytes and never shared between two documents.

    Refusals raise ``OntologyContractError`` (a ``ValueError``) with a stable
    ``code`` attribute: ``OntologySyntaxError`` for a malformed or non-UTF-8
    document, ``OntologyResourceError`` for an exceeded budget, and the base
    class itself for an RDF 1.2 triple term, which has no text form. The parse
    runs with the GIL released.
    """
    return _native.parse_triples(
        source,
        max_source_bytes=max_source_bytes,
        max_statements=max_statements,
    )


def parse_terms(
    source: bytes,
    *,
    max_source_bytes: int | None = None,
    max_statements: int | None = None,
) -> list[tuple[str, str, str, str, str | None, str | None]]:
    """Return every triple in the lossless term view.

    Each row is ``(subject, predicate, object, kind, datatype, language)``.
    IRIs are full, never abbreviated, and ``rdf:type`` stays a full IRI.
    ``kind`` is ``"iri"``, ``"blank"``, or ``"literal"``; a blank node renders a
    document-scoped ``_:label``, identical on every parse of the same bytes. For a literal, ``datatype`` is the
    datatype IRI of a typed literal and ``language`` the lowercased language
    tag; both are ``None`` for a simple literal and for IRI or blank-node
    objects.

    Where ``parse_triples`` renders the routing core's frozen text contract,
    this view keeps what the contract erases, so an app surface can rebuild its
    current graph term for term instead of re-parsing with a second reader.
    One rendering differs from rdflib by construction: a literal written
    ``"..."^^xsd:string`` reads as a simple literal (RDF 1.2 Concepts §3.3 makes
    it the same term; rdflib keeps the datatype). Literals carrying an RDF 1.2
    base direction, RDF 1.2 triple terms, and named graphs raise
    ``OntologyContractError``.
    """
    return _native.parse_terms(
        source,
        max_source_bytes=max_source_bytes,
        max_statements=max_statements,
    )


def prefixes(source: bytes, *, max_source_bytes: int | None = None) -> dict[str, str]:
    """Return the document's own prefix declarations.

    The table is the Turtle parser's: ``@prefix`` and SPARQL-style ``PREFIX``
    both declare, a directive-looking line inside a literal does not, and a
    prefix bound twice reports its final binding, which is the one
    ``parse_triples`` renders through. A document that does not parse is
    refused as ``parse_triples`` refuses it.
    """

    return _native.prefixes(source, max_source_bytes=max_source_bytes)


def query(
    source: bytes,
    patterns: list[tuple[str | None, str | None, str | None]],
    *,
    max_source_bytes: int | None = None,
    max_statements: int | None = None,
) -> list[tuple[str, str, str]]:
    """Answer `(s, p, o)` patterns; ``None`` is a wildcard.

    At least one position per pattern must be bound. The document is parsed
    once per call, so a caller with several patterns passes them together
    rather than re-reading the file.
    """
    return _native.query(
        source,
        patterns,
        max_source_bytes=max_source_bytes,
        max_statements=max_statements,
    )


__all__ = [
    "OntologyContractError",
    "OntologyResourceError",
    "OntologySyntaxError",
    "parse_terms",
    "parse_triples",
    "prefixes",
    "query",
]
