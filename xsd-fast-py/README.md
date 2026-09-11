# XSD Fast

`xsd-fast-py` is Atlas Brain's read-only Python binding for bounded XSD
introspection, Arrow field derivation, and XML validation.

## Contract

- **Input:** an XSD path or bytes for discovery/description, and XSD plus XML
  path/bytes for validation; include, depth, cardinality, facet, and error
  budgets are explicit.
- **Output:** global element names, field descriptions, validation findings, or
  Arrow IPC bytes describing a selected element schema.
- **Arrow boundary:** `schema_ipc_bytes` transports the derived schema
  description. It is not a signed production table schema and does not turn a
  validated XML document into a governed observation.
- **Authority:** successful XSD parsing or XML validation proves conformance to
  the supported schema checks only. It does not establish source ownership,
  business correctness, metric meaning, or schema approval.

The schema/domain owner owns XSD meaning and version approval; the source owner
owns document provenance; the parser maintainer owns deterministic parsing and
validation behavior. See
[information ownership](../../docs/atlas/atlas-information-ownership.md) and
the [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).

Unsupported schema behavior, include/depth limits, malformed XML, and bounded
validation findings must remain explicit. A pipeline must bind the source and
schema versions before any separate fact-promotion decision.

The approved proof-carrying OWL2-RL/Datalog reasoner is downstream of that
promotion boundary; this parser neither evaluates nor signs semantics.

## Verify

```bash
cargo test --manifest-path office-parsers-rs/Cargo.toml -p xsd-fast-py
cd app && uv run python scripts/verify_upload.py
```

Licensed under MIT.
