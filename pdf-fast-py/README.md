# PDF Fast

`pdf-fast-py` is the Python binding for native PDF text, layout, table,
page-geometry, bitmap, and raster extraction, on top of the native
`pdf-fast-core` parser. It can accept separately supplied OCR runs for table
reconstruction; native text extraction does not silently invoke an OCR
service.

## Contract

- **Input:** a PDF path plus explicit page, layout, table, DPI, or diagnostics
  options for the selected operation.
- **Output:** text/JSON projections, PNG page rasters, or explicit Arrow
  `RecordBatch` data depending on the API.
- **Arrow boundary:** semantic runs, multimodal page data, and table/layout
  surfaces expose Arrow IPC or owned Arrow C Data/PyCapsule paths. Deprecated
  raw-pointer helpers require explicit release; new code should prefer the
  owned scanner/capsule APIs.
- **Determinism:** parsed text, coordinates, tables, and images are exact for
  the same bytes. Extraction quality and layout reconstruction are the
  parser's best effort; interpretation and validation are the caller's
  responsibility.

Malformed PDFs, invalid pages, Arrow import errors, and bounded-resource
failures are surfaced to the caller. Diagnostics and fallbacks must remain
visible; callers must not promote partial or low-quality extraction merely
because the parser returned content.

## Verify

```bash
cargo test -p pdf-fast-py
```

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.