# XSD Fast

`xsd-fast-py` is the Python binding for bounded XSD introspection, Arrow field
derivation, and XML validation, on top of the native `xsd-fast-core` parser.

## Contract

- **Input:** an XSD path or bytes for discovery/description, and XSD plus XML
  path/bytes for validation; include, depth, cardinality, facet, and error
  budgets are explicit.
- **Output:** global element names, field descriptions, validation findings, or
  Arrow IPC bytes describing a selected element schema.
- **Arrow boundary:** `schema_ipc_bytes` transports the derived schema
  description; it is a description of a shape, not a signed production schema.
- **Determinism:** successful parsing or validation proves conformance to the
  supported checks only. Business meaning and schema approval remain the
  caller's responsibility.

Unsupported schema behavior, include/depth limits, malformed XML, and bounded
validation findings are surfaced to the caller rather than silently accepted.

## Verify

```bash
cargo test -p xsd-fast-py
```

Licensed under MIT.