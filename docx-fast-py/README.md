# DOCX Fast

`docx-fast-py` is the Python binding for deterministic DOCX structure
extraction, on top of the native `docx-fast-core` parser.

## Contract

- **Input:** a DOCX filesystem path or in-memory bytes, with an optional
  decompression budget.
- **Output:** an in-memory Python dictionary decoded from the native parser's
  structured JSON result. The binding writes no extracted files.
- **Arrow boundary:** this binding does not claim a native Arrow result today;
  a consumer maps the parsed projection into an explicit typed schema before
  any Arrow IPC transport.
- **Determinism:** parsing the same bytes always yields the same result.
  Successful parsing proves only that the accepted bytes were structurally
  readable; interpreting the content is the caller's responsibility.

Malformed archives, unsafe paths, and exceeded expansion limits fail instead
of producing a partial result. If provenance matters downstream, the caller
should keep the source hash alongside the parsed output.

## Verify

```bash
cargo test -p docx-fast-py
```

Licensed under MIT.