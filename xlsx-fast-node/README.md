# xlsx-fast-node

High-performance Excel (.xlsx, .xls, .xlsb) parser with a native Rust core.

## Features

- **Fast**: SIMD-optimized cell reference parsing (NEON on ARM, AVX2/SSE2 on x86)
- **XLSX path**: Memory-mapped files and bounded part decompression
- **Typed cells**: Cached formula results, optional formula text, and 1900/1904 dates
- **Safe**: Rust's memory safety guarantees
- **Output**: Workbook JSON, NDJSON, or Arrow IPC with the same cell schema

## Installation

```bash
pnpm add xlsx-fast-node
```

## Quick Start

```javascript
const { parseExcel, parseExcelNdjson } = require('xlsx-fast-node');

// Parse entire workbook
const workbook = parseExcel('/path/to/file.xlsb');
console.log(`Sheets: ${workbook.sheets.length}`);

for (const sheet of workbook.sheets) {
  console.log(`Sheet: ${sheet.name} (${sheet.cells.length} cells)`);
  for (const cell of sheet.cells) {
    console.log(`  [${cell.row},${cell.col}] = ${JSON.stringify(cell.value)}`);
  }
}
```

## Options

```javascript
const workbook = parseExcel('/path/to/file.xlsb', {
  decodeDates: true,       // Convert Excel dates to ISO8601 (default: true)
  includeFormulas: false,  // Include formula text (default: false)
  rawValues: false,        // Skip numeric/date parsing (default: false)
  maxInflate: 128 * 1024 * 1024,  // Expansion/allocation budget (default: 128 MiB)
});
```

## NDJSON Streaming

XLSX emits cells incrementally. XLS/XLSB materialize the workbook under resource
budgets before output:

```javascript
parseExcelNdjson('/path/to/large.xlsx', '/tmp/out.ndjson');
```

The NDJSON output format (one JSON object per line):
```json
{"kind":"sheet","id":1,"name":"Sheet1"}
{"kind":"cell","sheet_id":1,"row":1,"col":1,"type":"string","v":"Hello"}
{"kind":"cell","sheet_id":1,"row":1,"col":2,"type":"number","v":42}
{"kind":"cell","sheet_id":1,"row":1,"col":3,"type":"date","iso":"2024-01-15","serial":45276}
```

## Data Types

### CellValue

Cells can have one of these value types:

- `Empty` - Empty cell
- `Bool { v: boolean }` - Boolean value
- `Number { v: number }` - Numeric value
- `String { v: string }` - String value (from shared strings)
- `Raw { v: string }` - Raw string value (inline)
- `Error { v: string }` - Error value
- `Date { iso: string, serial: number }` - Date value (ISO8601 string + Excel serial)

## TypeScript Support

TypeScript definitions are included:

```typescript
import { parseExcel, ParseOptions, Workbook, Sheet, Cell } from 'xlsx-fast-node';

const workbook: Workbook = parseExcel('data.xlsx');
```

## Binary workbook compatibility

The existing `parseXlsx*` names remain aliases accepting all three formats.
Detection uses contents, not filename extensions. Hidden worksheets are retained
in workbook order; chart tabs are skipped. Encrypted files are unsupported and
formulas/macros are never executed. Formula text is opt-in.

`maxInflate` bounds aggregate ZIP expansion, XLS input/dense-range estimates, and
estimated retained binary cells separately. It is not a process-wide memory cap.
Binary raw strings are reconstructed from decoded values; duration-formatted
cells retain numeric serials. See [compatibility notes](../SPEC_COMPLIANCE.md).

## Platform Support

Prebuilt binaries available for:
- macOS (Intel & Apple Silicon)
- Linux (x64, ARM64)
- Windows (x64)

## License

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.

## Related Packages

- `docx-fast-node` - Word (.docx) parser
- `pptx-fast-node` - PowerPoint (.pptx) parser
