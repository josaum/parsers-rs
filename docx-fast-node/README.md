# docx-fast-node

High-performance Word (.docx) parser with a native Rust core.

## Features

- **Complete**: Extracts paragraphs, tables, headers, footers, footnotes, endnotes, comments
- **Styled**: Paragraph and character styles with inheritance
- **Lists**: Numbering support (bullets and numbered lists)
- **Track Changes**: Revision tracking support
- **Comments**: Threaded comments with author information
- **TOC**: Automatic table of contents generation from headings
- **Images**: Extract embedded images
- **Arrow IPC**: Export to Apache Arrow format for data processing
- **Fast**: Memory-mapped files and streaming decompression
- **Safe**: Rust's memory safety guarantees

## Installation

```bash
pnpm add docx-fast-node  # or the @mycelia/docx-fast-node consumer shell
```

## Quick Start

```javascript
const { parseDocx } = require('docx-fast-node');

const doc = parseDocx('/path/to/document.docx');

// Metadata
console.log('Title:', doc.metadata.core.title);
console.log('Author:', doc.metadata.core.creator);
console.log('Pages:', doc.page_count);

// Table of contents
for (const entry of doc.toc) {
  console.log('  '.repeat(entry.level) + entry.text);
}

// Document content
for (const block of doc.blocks) {
  if (block.type === 'paragraph') {
    console.log(`[${block.style_name || 'Normal'}] ${block.text}`);
  } else if (block.type === 'table') {
    for (const row of block.rows) {
      const cells = row.cells.map(cell => {
        // Extract text from cell blocks
        return cell.blocks
          .filter(b => b.type === 'paragraph')
          .map(p => p.text)
          .join(' ');
      });
      console.log(cells.join(' | '));
    }
  }
}
```

## Options

```javascript
const doc = parseDocx('/path/to/document.docx', {
  maxInflate: 128 * 1024 * 1024,  // Max decompressed size in bytes (default: 128 MiB)
});
```

## NDJSON Streaming

```javascript
const { parseDocxNdjson } = require('docx-fast-node');

parseDocxNdjson('/path/to/document.docx', '/tmp/out.ndjson');
```

Output format (one JSON object per line):
```json
{"kind":"metadata","metadata":{"core":{"title":"My Doc"}},"settings":{},"toc":[]}
{"kind":"style","style":{"id":"Heading1","name":"Heading 1","is_default":false}}
{"kind":"block","block":{"type":"paragraph","text":"Hello World"}}
```

## Arrow IPC Export

Export document tables to Apache Arrow format:

```javascript
const { parseDocxArrowDataset } = require('docx-fast-node');

// Export all tables to Arrow IPC files
const files = parseDocxArrowDataset('/path/to/doc.docx', '/tmp/arrow-output');
console.log('Created:', files);
```

## Image Extraction

```javascript
const { extractDocxImages } = require('docx-fast-node');

const imagePaths = extractDocxImages('/path/to/document.docx', './extracted-images');
console.log('Extracted images:', imagePaths);
```

## TypeScript Support

TypeScript definitions are included:

```typescript
import { parseDocx, DocxDocument, Paragraph, Table } from 'docx-fast-node';

const doc: DocxDocument = parseDocx('document.docx');
```

## Document Structure

```
DocxDocument
├── metadata (core, app, custom properties)
├── settings (document settings)
├── page_count
├── toc (auto-generated table of contents)
├── styles (paragraph, character, table styles)
├── numbering (list definitions)
├── blocks (paragraphs and tables)
│   ├── Paragraph { runs: [Run { text, bold, italic, ... }] }
│   └── Table { rows: [Row { cells: [Cell { blocks }] }] }
├── headers / footers
├── footnotes / endnotes
├── comments (with thread support)
├── revisions (track changes)
├── fields (TOC, page numbers, etc.)
└── images (embedded references)
```

## Performance

- **10x faster** than python-docx
- **Low memory footprint**: Streaming decompression for large files
- **Parallel-ready**: Stateless parsing allows concurrent processing

## Platform Support

Prebuilt binaries available for:
- macOS (Intel & Apple Silicon)
- Linux (x64, ARM64)
- Windows (x64)

## License

MIT

## Related Packages

- `xlsx-fast-node` - Excel (.xlsx) parser
- `pptx-fast-node` - PowerPoint (.pptx) parser
