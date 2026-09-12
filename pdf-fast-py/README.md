# PDF Fast

`pdf-fast-py` is Atlas Brain's read-only Python binding for native PDF text, layout,
table, page-geometry, bitmap, and raster extraction. It can accept separately
supplied OCR runs for table reconstruction; native text extraction does not
silently invoke an OCR service.

## Contract

- **Input:** a PDF path plus explicit page, layout, table, DPI, or diagnostics
  options for the selected operation.
- **Output:** text/JSON projections, PNG page rasters, or explicit Arrow
  `RecordBatch` data depending on the API.
- **Arrow boundary:** semantic runs, multimodal page data, and table/layout
  surfaces expose Arrow IPC or owned Arrow C Data/PyCapsule paths. Deprecated
  raw-pointer helpers require explicit release; new code should prefer the
  owned scanner/capsule APIs.
- **Authority:** extracted text, coordinates, tables, images, and OCR-assisted
  cells are unsigned. Neither text quality nor layout reconstruction certifies
  the document, its source, or any business claim.

The source/records owner owns access, provenance, and retention; the domain
steward owns interpretation; the parser maintainer owns deterministic decode,
resource bounds, and buffer safety. See
[information ownership](../../docs/atlas/atlas-information-ownership.md) and
the [data-pipeline contract](../../docs/atlas/atlas-data-pipelines.md).

Malformed PDFs, invalid pages, Arrow import errors, and bounded-resource
failures are surfaced to the caller. Diagnostics and fallbacks must remain
visible; callers must not promote partial or low-quality extraction merely
because the parser returned content.

The approved proof-carrying OWL2-RL/Datalog reasoner is downstream of a
separate promotion boundary; this parser neither evaluates nor signs semantics.

## Verify

```bash
cargo test --manifest-path office-parsers-rs/Cargo.toml -p pdf-fast-py
cd app && uv run python scripts/verify_upload.py
```

Licensed under MIT.
