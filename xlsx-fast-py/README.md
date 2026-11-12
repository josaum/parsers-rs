# XLSX Fast

`xlsx-fast-py` is the read-only Python binding for deterministic XLSX
(spreadsheet) extraction. It is a generic parser: the same call produces the
same workbook projection in every consumer.

## Contract

- **Input:** workbook bytes (in-memory) or a workbook path, with explicit
  options for date decoding, formula retention, raw values, and a decompression
  budget.
- **Output:** a structured workbook dictionary, or NDJSON / Arrow IPC written to
  a path. The binding writes no extracted files and does not modify the input.
- **Streaming:** `parse_xlsx_ndjson` and `parse_xlsx_arrow_ipc` exist for
  workbooks too large to want to materialize in memory. They write to a caller's
  path and return nothing; the file is the result.
- **Authority:** every result is unsigned. Successful parsing proves only that
  the accepted bytes were structurally readable as a workbook; it does not
  establish source ownership, cell meaning, formula correctness, or business
  interpretation.

Malformed archives, unsafe part paths, and exceeded expansion limits fail
loudly instead of yielding a partially correct workbook. Callers must preserve
the source hash and treat extracted cell values as unsigned until a separate
validation and promotion decision.

## Install

```bash
pip install xlsx_fast==0.2.1 --hash=sha256:<from dist/release-manifest.json>
```

Pin the version and verify the hash from `dist/release-manifest.json`. Every
wheel carries exactly one CycloneDX SBOM describing the crate that defines its
dependencies.

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
