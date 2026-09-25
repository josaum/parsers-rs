# Email Fast

`email-fast-py` is the Python binding for deterministic EML and MSG extraction,
on top of the native `email-fast-core` parser.

## Contract

- **Input:** an EML or MSG path, or in-memory bytes with an explicit format;
  callers may bound attachment size and opt into raw headers, HTML-to-text, or
  attachment-content extraction.
- **Output:** an in-memory Python dictionary containing parsed metadata and
  permitted content. The binding sends no message and writes no attachment.
- **Arrow boundary:** the current binding returns a structured Python object;
  a consumer applies an explicit schema before Arrow IPC transport or any
  downstream processing.
- **Determinism:** parsing the same bytes always yields the same result.
  Extraction does not verify identity, consent, source ownership, or delivery
  authenticity; those remain the caller's responsibility.

Invalid messages, unsupported structures, and attachment-budget violations
fail loudly rather than producing a partial result. Callers should preserve
source provenance when it matters downstream.

## Verify

```bash
cargo test -p email-fast-py
```

Licensed under MIT.