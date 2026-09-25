# json-fast

This is the current crate guide for the Python bindings to
[`json-fast-core`](../json-fast-core). The binding accepts JSON/JSONL text and
returns Python values or validation findings.

The binding does not call models or external services; it is a deterministic
parse and validation layer. Parsed values, JSON-LD metadata, JWT structure,
and schema findings are uninterpreted output: meaning and validation policy
are the caller's responsibility.

Arrow sized-batch conversion is not a shortcut past an explicit typed
contract: a pipeline must bind an explicit schema before Arrow IPC/Flight
transport. JSON-LD context expansion, JSON Schema validation, and the
JSON-RPC/JSON:API helpers stay within this crate's parsed surface.

## Develop locally

```bash
maturin develop
python3 -c "import json_fast; print(json_fast.json_type('{\"ok\":true}'))"
```

## Example

```python
from json_fast import count_lines, json_type, loads, loads_many, validate

record = loads('{"id":"sample-1","active":true}')
assert record["id"] == "sample-1"
assert json_type('{"a":1}') == "object"

jsonl = (
    '{"id":"a1","value":3.1}\n'
    '{"id":"a2","value":0.02}\n'
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
JSON Schema validation covers the draft-07 through 2020-12 constraint keywords
(validation, applicator and unevaluated vocabularies, local `$ref`, boolean
schemas, and a bounded `format` assertion set). Dynamic refs, remote refs,
unknown formats, and unknown keywords are surfaced as warnings, never silent
passes. BSON, JSON5, and HJSON remain out of scope. Parse and validation
failures remain caller-visible rather than yielding a partial result.

## Verify

```bash
cargo test -p json-fast-py
```