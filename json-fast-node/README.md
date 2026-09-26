# json-fast-node

This is the current crate guide for the Node.js N-API bindings to
[`json-fast-core`](../json-fast-core). See the
[repository README](../../README.md) for Atlas Brain's product contract and the
[transport overview](../../docs/atlas/json-fast-arrow.md) for the shared data
plane.

The package provides native JSON/JSONL parsing and focused helpers for JSON
Schema, JSON-RPC 2.0, JSON:API, JWT structure, and JSON-LD metadata. It is a
server-side/native binding, not a browser transport, identity verifier, or AI
gateway policy engine.

The binding accepts JSON/JSONL text and returns native JavaScript values or
validation findings. It has no Arrow transport surface today; a caller must
apply an explicit schema before moving a projection over Arrow IPC/Flight or
presenting it for governed promotion. All parsed values and helper results are
unsigned. JWT structure parsing does not verify identity, JSON-LD metadata does
not approve ontology meaning, and schema validation does not certify business
truth. See [information ownership](../../docs/atlas/atlas-information-ownership.md)
and the [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).
The approved proof-carrying reasoner is downstream of separate fact/rule
promotion; this binding never evaluates or strengthens semantic assurance.

## Build

```bash
cd office-parsers-rs/json-fast-node
node build.mjs          # or: node build.mjs --debug
```

`build.mjs` runs `cargo build --locked --release -p json-fast-node` and copies
the resulting library (`libjson_fast_node.dylib`, `libjson_fast_node.so`, or
`json_fast_node.dll`) to `json_fast_node.node`, the one file `index.js` loads.
The package has no npm dependencies, so there is nothing to install and no
N-API CLI involved; `pnpm run build` and `pnpm test` call the same scripts.
`json_fast_node.node` is a build output: do not commit it.

## Example

```js
const {
  countLines,
  jsonType,
  parseJson,
  parseJsonl,
  validate,
} = require("json-fast-node");

const record = parseJson('{"brand":"Nylabone","active":true}');
if (record.brand !== "Nylabone") throw new Error("unexpected brand");

const jsonl =
  '{"metric_id":"roas","value":3.1}\n' +
  '{"metric_id":"ad_ctr","value":0.02}\n';
const rows = parseJsonl(jsonl);
if (rows.length !== 2 || countLines(jsonl) !== 2) {
  throw new Error("unexpected JSONL result");
}

if (jsonType('{"a":1}') !== "object") throw new Error("unexpected type");

const issues = validate(
  '{"value":3.1}',
  '{"type":"object","properties":{"value":{"type":"number"}}}',
);
if (issues.some((issue) => issue.severity === "error")) {
  throw new Error("schema validation failed");
}
```

The validation helper implements a focused, fail-loud subset. Unsupported
keywords are not silently treated as verified. BSON, JSON5, and HJSON are out
of scope; use format-specific libraries for those formats. Parse and
validation failures remain caller-visible rather than yielding a partial
governed record.

## Verify

```bash
cargo test --locked --manifest-path office-parsers-rs/Cargo.toml -p json-fast-node
cd office-parsers-rs/json-fast-node && node build.mjs --debug && node --test smoke.test.mjs
```

The Rust unit tests call the exported functions as plain Rust and need no Node
runtime. They do not cover the generated N-API glue, the camelCase export
names, or JS value conversion; `smoke.test.mjs` covers those through
`index.js` and needs Node 18 or newer plus a built addon. It is not part of
`scripts/verify-atlas.sh`.
