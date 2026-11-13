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

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
## Every element's Arrow schema in one call

```python
import xsd_fast

schemas = xsd_fast.all_arrow_schemas("schema.xsd")
for element, fields in schemas.items():
    print(element, [(f["path"], f["arrow_type"], f["nullable"]) for f in fields])
```

Each value has the shape `describe_element` returns for one element. If any
element's schema cannot be derived the call raises `RuntimeError` instead of
leaving that element out.
