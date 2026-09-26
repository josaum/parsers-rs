"""Boundary contract of the ``ontology_fast`` binding.

The reader's behaviour is pinned by the Rust tests in ``ontology-fast-core``;
these pin what only exists on the Python side: row shapes, keyword arguments,
exception types and their ``code`` attribute, and that a parse does not hold
the GIL. They use small inline documents and no repository corpus.

They exercise whichever ``ontology_fast`` extension is importable, so they say
nothing about the Rust source until that extension has been rebuilt from it.
"""

from __future__ import annotations

import threading
import time

import ontology_fast
import pytest
from ontology_fast import (
    OntologyContractError,
    OntologyResourceError,
    OntologySyntaxError,
)

DOC = (
    b"@prefix atlas: <https://atlas.example/ns#> .\n"
    b"@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n"
    b'atlas:ROAS a atlas:Metric ; atlas:label "Return"@EN ;\n'
    b'  atlas:since "2026-09-16"^^xsd:date ; atlas:note "plain" ;\n'
    b"  atlas:owner [ atlas:name \"revops\" ] .\n"
)
NS = "https://atlas.example/ns#"


def test_parse_triples_returns_text_tuples() -> None:
    rows = ontology_fast.parse_triples(DOC)
    assert type(rows) is list
    assert all(type(row) is tuple and len(row) == 3 for row in rows)
    assert ("atlas:ROAS", "a", "atlas:Metric") in rows
    assert ("atlas:ROAS", "atlas:label", "Return") in rows
    assert len(rows) == 6


def test_parse_terms_returns_six_tuples_with_kinds_and_typing() -> None:
    rows = ontology_fast.parse_terms(DOC)
    assert all(type(row) is tuple and len(row) == 6 for row in rows)
    by_predicate = {row[1]: row for row in rows if row[0] == f"{NS}ROAS"}
    assert by_predicate["http://www.w3.org/1999/02/22-rdf-syntax-ns#type"][2:] == (
        f"{NS}Metric",
        "iri",
        None,
        None,
    )
    assert by_predicate[f"{NS}label"][2:] == ("Return", "literal", None, "en")
    assert by_predicate[f"{NS}since"][2:] == (
        "2026-09-16",
        "literal",
        "http://www.w3.org/2001/XMLSchema#date",
        None,
    )
    assert by_predicate[f"{NS}note"][2:] == ("plain", "literal", None, None)
    owner = by_predicate[f"{NS}owner"]
    assert owner[3] == "blank" and owner[2].startswith("_:")
    assert [row for row in rows if row[0] == owner[2]], "the blank node is a subject too"


def test_blank_labels_are_stable_across_calls_and_distinct_across_documents() -> None:
    assert ontology_fast.parse_terms(DOC) == ontology_fast.parse_terms(DOC)
    assert ontology_fast.parse_triples(DOC) == ontology_fast.parse_triples(DOC)
    one = ontology_fast.parse_terms(b'_:r <urn:p> "one" .\n')
    two = ontology_fast.parse_terms(b'_:r <urn:p> "two" .\n')
    assert one[0][0] != two[0][0]


def test_prefixes_come_from_the_parser_not_from_the_text() -> None:
    source = (
        b"PREFIX sparql: <urn:sparql:>\n"
        b"@prefix at: <urn:at:> .\n"
        b'at:s at:p """\n@prefix bogus: <urn:bogus:> .\n""" .\n'
    )
    assert ontology_fast.prefixes(source) == {"sparql": "urn:sparql:", "at": "urn:at:"}


def test_query_selects_by_any_bound_position() -> None:
    rows = ontology_fast.query(
        DOC, [("atlas:ROAS", "a", None), (None, None, "plain"), ("atlas:absent", None, None)]
    )
    assert rows == [
        ("atlas:ROAS", "a", "atlas:Metric"),
        ("atlas:ROAS", "atlas:note", "plain"),
    ]


@pytest.mark.parametrize(
    "patterns",
    [[(None, None, None)], [("a", "b")], ["not-a-tuple"]],
)
def test_query_refuses_malformed_patterns(patterns: list) -> None:
    with pytest.raises((ValueError, TypeError)):
        ontology_fast.query(DOC, patterns)


@pytest.mark.parametrize("call", ["parse_triples", "parse_terms", "prefixes"])
def test_malformed_documents_raise_the_syntax_subclass(call: str) -> None:
    for source in (b"<urn:s> <urn:p> .\n", b"# \xff\n"):
        with pytest.raises(OntologySyntaxError) as raised:
            getattr(ontology_fast, call)(source)
        assert raised.value.code == "syntax"
        assert isinstance(raised.value, OntologyContractError)
        assert isinstance(raised.value, ValueError)


@pytest.mark.parametrize("call", ["parse_triples", "parse_terms", "prefixes"])
def test_the_source_budget_raises_the_resource_subclass(call: str) -> None:
    with pytest.raises(OntologyResourceError, match="source_bytes") as raised:
        getattr(ontology_fast, call)(DOC, max_source_bytes=len(DOC) - 1)
    assert raised.value.code == "resource_limit"


@pytest.mark.parametrize("call", ["parse_triples", "parse_terms"])
def test_the_statement_budget_raises_the_resource_subclass(call: str) -> None:
    with pytest.raises(OntologyResourceError, match="statements"):
        getattr(ontology_fast, call)(DOC, max_statements=1)
    with pytest.raises(OntologyResourceError, match="statements"):
        ontology_fast.query(DOC, [("atlas:ROAS", None, None)], max_statements=1)


def test_unrepresentable_terms_raise_the_base_contract_error() -> None:
    triple_term = b"<urn:s> <urn:p> <<( <urn:a> <urn:b> <urn:c> )>> .\n"
    for call in (ontology_fast.parse_triples, ontology_fast.parse_terms):
        with pytest.raises(OntologyContractError) as raised:
            call(triple_term)
        assert type(raised.value) is OntologyContractError
        assert raised.value.code == "triple_term"
    with pytest.raises(OntologyContractError) as raised:
        ontology_fast.parse_terms(b'<urn:s> <urn:p> "x"@en--ltr .\n')
    assert raised.value.code == "base_direction"


def test_exceptions_live_in_an_importable_module() -> None:
    for cls in (OntologyContractError, OntologySyntaxError, OntologyResourceError):
        assert cls.__module__ == "ontology_fast"
        assert getattr(ontology_fast, cls.__name__) is cls


@pytest.mark.parametrize("source", ["text", bytearray(b"<urn:s> <urn:p> <urn:o> .")])
def test_only_bytes_are_accepted(source: object) -> None:
    with pytest.raises(TypeError):
        ontology_fast.parse_triples(source)  # type: ignore[arg-type]


def longest_main_thread_stall(call) -> tuple[float, float]:
    """Run ``call`` on a worker; return (its duration, the main thread's longest stall)."""
    done = threading.Event()
    duration: list[float] = []

    def work() -> None:
        begin = time.perf_counter()
        call()
        duration.append(time.perf_counter() - begin)
        done.set()

    worker = threading.Thread(target=work)
    longest = 0.0
    # The clock starts before the worker does: a call that holds the GIL keeps
    # this thread from running at all, so any later starting point is reached
    # only once the stall is already over.
    previous = time.perf_counter()
    worker.start()
    finished = False
    while not finished:
        # The flag is read first so the stall that ends with the worker
        # finishing, which is the one that matters, is measured too.
        finished = done.is_set()
        now = time.perf_counter()
        longest = max(longest, now - previous)
        previous = now
    worker.join()
    return duration[0], longest


def test_a_parse_does_not_hold_the_gil() -> None:
    # `prefixes` parses the whole document and converts almost nothing, so the
    # call is all parse. If the parse held the GIL, the main thread would stall
    # for about as long as the call took; released, it never stalls for more
    # than a scheduling blip.
    body = b"@prefix ex: <urn:x:> .\n" + b"".join(
        b"ex:s%d ex:p \"value %d\" .\n" % (index, index) for index in range(300_000)
    )
    duration, stall = longest_main_thread_stall(lambda: ontology_fast.prefixes(body))
    assert duration > 0.02, f"document too small to measure: {duration:.4f}s"
    assert stall < duration / 2, f"main thread stalled {stall:.4f}s of a {duration:.4f}s parse"
