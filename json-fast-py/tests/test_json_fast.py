from __future__ import annotations

import json_fast
import pytest


def test_loads_exposes_typed_resource_limit_errors() -> None:
    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: input_bytes"):
        json_fast.loads('{"a": 1}', max_input_bytes=4)


def test_loads_many_accepts_configurable_line_and_record_budgets() -> None:
    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: lines"):
        json_fast.loads_many('{"a": 1}\n\n{"a": 2}\n', max_lines=2)

    with pytest.raises(json_fast.JsonResourceError, match="resource_limit: records"):
        json_fast.loads_many('{"a": 1}\n{"a": 2}\n', max_records=1)


def test_protocol_null_members_survive_native_round_trip() -> None:
    assert json_fast.jsonapi_document('{"data":null}') == {"data": None}
    assert json_fast.jsonrpc_response('{"jsonrpc":"2.0","result":null,"id":null}') == {
        "jsonrpc": "2.0", "result": None, "id": None,
    }
    assert "id" not in json_fast.jsonrpc_request('{"jsonrpc":"2.0","method":"notify"}')
    assert json_fast.jsonrpc_request('{"jsonrpc":"2.0","method":"call","id":null}')["id"] is None


@pytest.mark.parametrize("input", ['\u00a01\n', '\u000b\n'])
def test_jsonl_rejects_non_json_whitespace(input: str) -> None:
    with pytest.raises(RuntimeError):
        json_fast.loads_many(input)


def test_protocols_reject_invalid_envelopes() -> None:
    with pytest.raises(RuntimeError):
        json_fast.jsonapi_document('{"data":null,"errors":[]}')
    with pytest.raises(RuntimeError):
        json_fast.jsonrpc_response('{"jsonrpc":"2.0","result":1}')
    with pytest.raises(RuntimeError):
        json_fast.jsonrpc_request('{"jsonrpc":"2.0","method":"m","params":null}')
