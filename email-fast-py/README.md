# Email Fast

`email-fast-py` is Atlas Brain's read-only Python binding for deterministic EML and
MSG extraction. It belongs to the native ingestion plane, not the delivery,
identity, or AI Gateway plane.

## Contract

- **Input:** an EML or MSG path, or in-memory bytes with an explicit format;
  callers may bound attachment size and opt into raw headers, HTML-to-text, or
  attachment-content extraction.
- **Output:** an in-memory Python dictionary containing parsed metadata and
  permitted content. The binding sends no message and writes no attachment.
- **Arrow boundary:** the current binding returns a structured Python object.
  A pipeline must apply an explicit schema before Arrow IPC/Flight transport or
  promotion into a governed observation.
- **Authority:** all extracted sender, recipient, body, header, and attachment
  fields remain unsigned. Parsing does not verify identity, consent, source
  ownership, delivery authenticity, or business meaning.

The source/records owner is accountable for lawful access and retention; the
domain steward owns interpretation; the parser maintainer owns bounded,
deterministic extraction. See the
[information-ownership model](../../docs/atlas/atlas-information-ownership.md)
and [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).

Invalid messages, unsupported structures, and attachment-budget violations
fail loudly. Callers must retain source provenance and must not treat
extraction success as a signature or fact-promotion decision.

The approved proof-carrying OWL2-RL/Datalog reasoner is downstream of that
promotion boundary; this parser neither evaluates nor signs semantics.

## Verify

```bash
cargo test --manifest-path office-parsers-rs/Cargo.toml -p email-fast-py
cd app && uv run python scripts/verify_upload.py
```

Licensed under MIT.
