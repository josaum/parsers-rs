# json-fast

This is the current crate guide for the Python bindings to
[`json-fast-core`](../json-fast-core). See the
[repository README](../../README.md) for Atlas Brain's product contract and the
[transport overview](../../docs/atlas/json-fast-arrow.md) for the shared data
plane.

Atlas Brain uses this package as its canonical JSON/JSONL parser so product code does
not maintain a second fallback path with different behavior. The binding does
not call models, select providers, enforce DLP, or replace Pydantic validation
at the product boundary.

The binding accepts JSON/JSONL text and returns Python values or validation
findings. It does not expose the shared Arrow conversion as a Python authority
shortcut: a pipeline must bind an explicit typed contract before Arrow
IPC/Flight transport or fact promotion. Parsed values, JSON-LD metadata, JWT
structure, and schema findings are unsigned. The source owner owns provenance;
the domain/metric steward owns meaning; the parser maintainer owns deterministic
decode behavior. See
[information ownership](../../docs/atlas/atlas-information-ownership.md) and
the [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).
The approved proof-carrying reasoner is downstream of separate fact/rule
promotion; this binding never evaluates or strengthens semantic assurance.

## Develop locally

From this crate:

```bash
maturin develop
```

From the Atlas Brain repository root, `uv` builds the editable binding declared in
the root `pyproject.toml`:

```bash
uv sync --extra dev
uv run python -c "import json_fast; print(json_fast.json_type('{\"ok\":true}'))"
```

## Example

```python
from json_fast import count_lines, json_type, loads, loads_many, validate

record = loads('{"brand":"Nylabone","active":true}')
assert record["brand"] == "Nylabone"
assert json_type('{"a":1}') == "object"

jsonl = (
    '{"metric_id":"roas","value":3.1}\n'
    '{"metric_id":"ad_ctr","value":0.02}\n'
)
rows = loads_many(jsonl)
assert len(rows) == 2
assert count_lines(jsonl) == 2

issues = validate(
    '{"value":3.1}',
    '{"type":"object","properties":{"value":{"type":"number"}}}',
)
assert not any(issue["severity"] == "error" for issue in issues)
```

The package also exposes focused JSON-RPC, JSON:API, JWT, and JSON-LD helpers.
Schema validation is deliberately bounded and reports unsupported behavior
instead of implying full standards coverage. BSON, JSON5, and HJSON remain out
of scope. Parse and validation failures remain caller-visible rather than
yielding a partial governed record.

## Verify

```bash
cargo test --manifest-path office-parsers-rs/Cargo.toml -p json-fast-py
uv run --project . python -c "import json_fast; assert json_fast.loads('{\"ok\":true}')['ok']"
```
