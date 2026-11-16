# XLSX Fast

`xlsx-fast-py` is the read-only Python binding for deterministic XLSX, XLS and XLSB
spreadsheet extraction. It is a generic parser: the same call produces the
same workbook projection in every consumer.

## Contract

- **Input:** workbook bytes (in-memory) or a workbook path, with explicit
  options for date decoding, formula retention, raw values, and a decompression
  budget.
- **Output:** a structured workbook dictionary, or NDJSON / Arrow IPC written to
  a path. The binding writes no extracted files and does not modify the input.
- **Output paths:** `parse_excel_ndjson` and `parse_excel_arrow_ipc` write to a
  caller's path. XLSX emits cells incrementally; XLS/XLSB materialize under
  resource budgets before output. The Arrow function returns the byte count.
- **Authority:** every result is unsigned. Successful parsing proves only that
  the accepted bytes were structurally readable as a workbook; it does not
  establish source ownership, cell meaning, formula correctness, or business
  interpretation.

Malformed archives, unsafe part paths, and exceeded expansion limits fail
loudly instead of yielding a partially correct workbook. Callers must preserve
the source hash and treat extracted cell values as unsigned until a separate
validation and promotion decision.

## Read Excel files

```python
from pathlib import Path
import xlsx_fast

workbook = xlsx_fast.parse_excel("report.xls", include_formulas=True)
upload = xlsx_fast.parse_excel_bytes(Path("report.xlsb").read_bytes())
xlsx_fast.parse_excel_ndjson("report.xlsb", "cells.ndjson")
xlsx_fast.parse_excel_arrow_ipc("report.xls", "cells.arrow")
```

The existing `parse_xlsx*` names accept all three formats too. Detection uses file
contents. Worksheet order, hidden worksheets, dates in either epoch and cached
formula results use the shared cell contract. Formula text is opt-in; formulas
and macros are never executed. Chart tabs are skipped and encrypted files fail.

`max_inflate` bounds aggregate ZIP expansion and, for XLS, input size and estimated
dense-range allocations. Binary formats also cap estimated retained cells; it is
not a process-wide memory ceiling. Binary `raw_values` strings are reconstructed
from decoded values. Duration-formatted binary cells retain numeric serials.
See [compatibility notes](../SPEC_COMPLIANCE.md) for coverage and limits.

## Install

```bash
pip install xlsx_fast==0.2.6 --hash=sha256:<from dist/release-manifest.json>
```

Pin the version and verify the hash from `dist/release-manifest.json`. Every
wheel carries exactly one CycloneDX SBOM describing the crate that defines its
dependencies.

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
