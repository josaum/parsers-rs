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
