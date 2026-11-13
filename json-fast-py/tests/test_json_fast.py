"""Every `json_fast` entry point: a result, its errors, the GIL release and the stub.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import base64
import functools
import inspect
import json
import sys
import threading
from pathlib import Path

import json_fast
import pytest


def b64url(text: str) -> str:
    return base64.urlsafe_b64encode(text.encode("utf-8")).decode("ascii").rstrip("=")


# header {"alg":"none"} . payload {"sub":"a"} . empty signature
SMALL_JWT = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhIn0."


def test_loads_exposes_typed_resource_limit_errors() -> None:
    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: input_bytes"):
        json_fast.loads('{"a": 1}', max_input_bytes=4)


def test_loads_many_accepts_configurable_line_and_record_budgets() -> None:
    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: lines"):
        json_fast.loads_many('{"a": 1}\n\n{"a": 2}\n', max_lines=2)

    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: records"):
        json_fast.loads_many('{"a": 1}\n{"a": 2}\n', max_records=1)


def test_version_is_the_package_version() -> None:
    assert json_fast.version() == json_fast.__version__
    assert json_fast.__version__


def test_loads_returns_the_six_json_types_as_python_objects() -> None:
    value = json_fast.loads('{"s":"x","n":2.5,"i":-3,"u":18446744073709551615,"b":true,"z":null,"a":[1]}')
    assert value == {"s": "x", "n": 2.5, "i": -3, "u": 18446744073709551615, "b": True, "z": None, "a": [1]}
    assert json_fast.loads("[]", max_nesting_depth=4, max_tokens=8, max_retained_bytes=4096) == []


def test_loads_many_skips_blank_lines() -> None:
    assert json_fast.loads_many('{"a":1}\n\n{"a":2}\n') == [{"a": 1}, {"a": 2}]


def test_count_lines_counts_non_blank_lines() -> None:
    assert json_fast.count_lines('{"a":1}\n  \n{"a":2}\n') == 2
    assert json_fast.count_lines("") == 0


@pytest.mark.parametrize(
    "text,kind",
    [('"s"', "string"), ("1", "number"), ("true", "boolean"), ("null", "null"), ("[1]", "array"), ("{}", "object")],
)
def test_json_type_classifies_the_root(text: str, kind: str) -> None:
    assert json_fast.json_type(text) == kind


def test_validate_reports_findings_as_dicts() -> None:
    schema = '{"type":"object","properties":{"value":{"type":"number"}}}'
    assert not [f for f in json_fast.validate('{"value":3.1}', schema) if f["severity"] == "error"]
    errors = [f for f in json_fast.validate('{"value":"x"}', schema) if f["severity"] == "error"]
    assert errors and set(errors[0]) == {"severity", "path", "message"}


def test_validate_takes_supplied_schema_documents_and_fails_closed() -> None:
    schema = '{"$ref": "https://example.com/positive.json"}'
    resources = {"https://example.com/positive.json": '{"type": "integer", "minimum": 1}'}
    errors = lambda findings: [f for f in findings if f["severity"] == "error"]
    assert not errors(json_fast.validate("3", schema, resources=resources))
    assert errors(json_fast.validate("0", schema, resources=resources))
    # Nothing is fetched: without the document the reference fails closed.
    unresolved = errors(json_fast.validate("3", schema))
    assert unresolved and "does not resolve" in unresolved[0]["message"]
    with pytest.raises(RuntimeError):
        json_fast.validate("3", schema, resources={"https://example.com/positive.json": "{"})


def test_jsonrpc_envelopes_round_trip() -> None:
    assert json_fast.jsonrpc_request('{"jsonrpc":"2.0","method":"ping","id":1}')["method"] == "ping"
    assert json_fast.jsonrpc_response('{"jsonrpc":"2.0","result":7,"id":1}')["result"] == 7


def test_jsonapi_document_round_trips() -> None:
    assert json_fast.jsonapi_document('{"data":{"type":"t","id":"1"}}')["data"]["id"] == "1"


def test_jwt_parts_splits_without_verifying() -> None:
    parts = json_fast.jwt_parts(SMALL_JWT)
    assert parts == {"header": {"alg": "none"}, "payload": {"sub": "a"}, "signature": ""}


def test_jsonld_meta_returns_only_the_keys_present() -> None:
    meta = json_fast.jsonld_meta('{"@context":"https://schema.org","@type":"Thing"}')
    assert meta == {"context": "https://schema.org", "type": "Thing"}


@pytest.mark.parametrize(
    "name,args",
    [
        ("loads", ("{",)),
        ("loads_many", ('{"a":1}\nnot json\n',)),
        ("json_type", ("{",)),
        ("validate", ("{", "{}")),
        ("validate", ("{}", "{")),
        ("jsonrpc_request", ('{"jsonrpc":"1.0","method":"ping"}',)),
        ("jsonrpc_response", ('{"jsonrpc":"2.0","id":1}',)),
        ("jsonapi_document", ("{}",)),
        ("jwt_parts", ("not-a-jwt",)),
        ("jsonld_meta", ("[]",)),
    ],
)
def test_bad_documents_raise_runtime_error(name: str, args: tuple) -> None:
    with pytest.raises(RuntimeError):
        getattr(json_fast, name)(*args)


# Each function and a valid argument list; every argument is text.
TEXT_CALLS = [
    ("loads", ('{"a":1}',)),
    ("loads_many", ('{"a":1}\n',)),
    ("count_lines", ('{"a":1}\n',)),
    ("json_type", ("{}",)),
    ("validate", ("{}", "{}")),
    ("jsonrpc_request", ('{"jsonrpc":"2.0","method":"ping","id":1}',)),
    ("jsonrpc_response", ('{"jsonrpc":"2.0","result":1,"id":1}',)),
    ("jsonapi_document", ('{"meta":{}}',)),
    ("jwt_parts", (SMALL_JWT,)),
    ("jsonld_meta", ("{}",)),
]


@pytest.mark.parametrize("wrap", [bytes, bytearray, lambda raw: memoryview(bytearray(raw))])
@pytest.mark.parametrize(
    "name,args,position",
    [(name, args, i) for name, args in TEXT_CALLS for i in range(len(args))],
)
def test_buffers_are_refused_so_no_python_buffer_is_read_without_the_gil(
    name: str, args: tuple, position: int, wrap
) -> None:
    """Only `str` is accepted: no bytes-like object, mutable or not, reaches the parser."""
    function = getattr(json_fast, name)
    function(*args)  # the text form is valid
    as_buffer = list(args)
    as_buffer[position] = wrap(args[position].encode("utf-8"))
    with pytest.raises(TypeError):
        function(*as_buffer)


def times_another_thread_ran_during(call, attempts: int = 5) -> int:
    """How many of `attempts` calls let a second Python thread run meanwhile.

    The switch interval is raised so the interpreter never hands the GIL over
    between bytecodes of the calling thread. A second thread, made runnable
    just before `call`, can then run only if `call` releases the GIL itself:
    with the GIL held the count is exactly 0, whatever the machine's load.
    No duration is compared with a threshold.
    """
    previous = sys.getswitchinterval()
    sys.setswitchinterval(1000.0)
    count = 0
    try:
        for _ in range(attempts):
            go = threading.Event()
            ran: list = []

            def waiter() -> None:
                go.wait()
                ran.append(True)

            thread = threading.Thread(target=waiter)
            thread.start()  # returns once the waiter is blocked in go.wait()
            go.set()
            call()
            count += bool(ran)
            thread.join()
    finally:
        sys.setswitchinterval(previous)
    return count


def holds_the_gil() -> int:
    """A call that never releases the GIL: the control for the test below."""
    return sum(range(3_000_000))


@functools.lru_cache(maxsize=None)
def big_array() -> str:
    """About 5 MB and 100 000 tokens: inside every default budget."""
    return json.dumps(["x" * 40 + str(i) for i in range(100_000)])


@functools.lru_cache(maxsize=None)
def big_jsonl() -> str:
    return "".join('{"k":"%s"}\n' % ("y" * 40 + str(i)) for i in range(60_000))


# Each call parses megabytes in the core, long enough for a waiting thread to run.
GIL_CALLS = {
    "loads": lambda: json_fast.loads(big_array()),
    "loads_many": lambda: json_fast.loads_many(big_jsonl()),
    "count_lines": lambda: json_fast.count_lines(big_jsonl() * 8),
    "json_type": lambda: json_fast.json_type(big_array()),
    "validate": lambda: json_fast.validate(big_array(), '{"type":"array","items":{"type":"string"}}'),
    "jsonrpc_request": lambda: json_fast.jsonrpc_request(
        '{"jsonrpc":"2.0","method":"m","params":%s,"id":1}' % big_array()
    ),
    "jsonrpc_response": lambda: json_fast.jsonrpc_response('{"jsonrpc":"2.0","result":%s,"id":1}' % big_array()),
    "jsonapi_document": lambda: json_fast.jsonapi_document('{"meta":{"items":%s}}' % big_array()),
    "jwt_parts": lambda: json_fast.jwt_parts("%s.%s." % (b64url('{"alg":"none"}'), b64url('{"items":%s}' % big_array()))),
    "jsonld_meta": lambda: json_fast.jsonld_meta('{"@context":"https://schema.org","@graph":%s}' % big_array()),
}


def test_the_gil_control_never_lets_the_other_thread_run() -> None:
    assert times_another_thread_ran_during(holds_the_gil) == 0


@pytest.mark.parametrize("name", sorted(GIL_CALLS))
def test_every_core_call_releases_the_gil(name: str) -> None:
    call = GIL_CALLS[name]
    call()  # the input is valid and warms the cached text
    assert times_another_thread_ran_during(call) >= 1


def test_the_gil_proof_covers_every_function_that_takes_text() -> None:
    assert set(GIL_CALLS) == {name for name, _ in TEXT_CALLS}


def test_the_wheel_ships_type_information_that_matches_the_module() -> None:
    package_dir = Path(json_fast.__file__).parent
    assert (package_dir / "py.typed").is_file()
    functions = {
        name
        for name in dir(json_fast)
        if not name.startswith("_")
        and (inspect.isbuiltin(getattr(json_fast, name)) or inspect.isfunction(getattr(json_fast, name)))
    }
    assert {name for name in json_fast.__all__ if not name.startswith("_")} <= functions | {"JsonResourceError"}
    tree = ast.parse((package_dir / "__init__.pyi").read_text(encoding="utf-8"))
    declared = {node.name: node.args for node in tree.body if isinstance(node, ast.FunctionDef)}
    assert set(declared) == functions
    classes = {node.name for node in tree.body if isinstance(node, ast.ClassDef)}
    assert classes == {"JsonResourceError"}
    assert issubclass(json_fast.JsonResourceError, RuntimeError)
    for name, args in declared.items():
        real = inspect.signature(getattr(json_fast, name)).parameters.values()
        assert [a.arg for a in args.args] == [p.name for p in real if p.kind is p.POSITIONAL_OR_KEYWORD], name
        assert [a.arg for a in args.kwonlyargs] == [p.name for p in real if p.kind is p.KEYWORD_ONLY], name
