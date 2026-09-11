# DOCX Fast

`docx-fast-py` is Atlas Brain's read-only Python binding for deterministic DOCX
structure extraction. It is one parser in the native ingestion plane described
by the [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).

## Contract

- **Input:** a DOCX filesystem path or in-memory bytes, with an optional
  decompression budget.
- **Output:** an in-memory Python dictionary decoded from the native parser's
  structured JSON result. The binding writes no extracted files.
- **Arrow boundary:** this binding does not claim a native Arrow result today.
  A downstream pipeline must map the parsed projection into an explicit typed
  schema before Arrow IPC, Flight, or governed fact promotion.
- **Authority:** every result is unsigned. Successful parsing proves only that
  the accepted bytes were structurally readable; it does not establish source
  ownership, business correctness, metric meaning, or certification.

The source owner remains accountable for provenance and access, the domain or
metric steward for interpretation, and the parser maintainer for safe,
deterministic extraction. See
[information ownership](../../docs/atlas/atlas-information-ownership.md).

Malformed archives, unsafe paths, and exceeded expansion limits fail instead
of producing a partial governed record. Callers must preserve the source hash
and treat any extracted content as unsigned until a separate validation and
promotion decision.

The approved proof-carrying OWL2-RL/Datalog reasoner is downstream of that
promotion boundary; this parser neither evaluates nor signs semantics.

## Verify

```bash
cargo test --manifest-path office-parsers-rs/Cargo.toml -p docx-fast-py
cd app && uv run python scripts/verify_upload.py
```

Licensed under MIT.
