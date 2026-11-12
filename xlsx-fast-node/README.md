# xlsx-fast-node

High-performance Excel (.xlsx) parser with a native Rust core.

## Features

- **Fast**: SIMD-optimized cell reference parsing (NEON on ARM, AVX2/SSE2 on x86)
- **Memory Efficient**: Memory-mapped files and streaming decompression
- **Complete**: Handles formulas, dates (1900/1904 systems), shared strings
- **Safe**: Rust's memory safety guarantees
- **NDJSON Streaming**: For processing huge files without loading into memory

## Installation

```bash
pnpm add xlsx-fast-node
```

## Quick Start

```javascript
const { parseXlsx, parseXlsxNdjson } = require('xlsx-fast-node');

// Parse entire workbook
const workbook = parseXlsx('/path/to/file.xlsx');
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
const workbook = parseXlsx('/path/to/file.xlsx', {
  decodeDates: true,       // Convert Excel dates to ISO8601 (default: true)
  includeFormulas: false,  // Include formula text (default: false)
  rawValues: false,        // Skip numeric/date parsing (default: false)
  maxInflate: 128 * 1024 * 1024,  // Max decompressed size in bytes (default: 128 MiB)
});
```

## NDJSON Streaming

For large files, stream to NDJSON without loading everything in memory:

```javascript
parseXlsxNdjson('/path/to/large.xlsx', '/tmp/out.ndjson');
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
import { parseXlsx, ParseOptions, Workbook, Sheet, Cell } from 'xlsx-fast-node';

const workbook: Workbook = parseXlsx('data.xlsx');
```

## Performance

Benchmarks show significant performance improvements over pure JavaScript/Node.js alternatives:

- **10x faster** than openpyxl (Python)
- **5x faster** than SheetJS in streaming mode
- Uses **50% less memory** for large files

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
