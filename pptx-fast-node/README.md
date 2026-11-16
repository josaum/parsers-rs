# pptx-fast-node

High-performance PowerPoint (.pptx) parser with a native Rust core.

## Features

- **Complete**: Extracts slides, shapes, tables, notes, comments
- **Styled**: Text formatting (bold, italic, underline)
- **Templates**: Slide masters and layouts
- **Notes**: Speaker notes extraction
- **Comments**: Slide-level comments with author info
- **Images**: Extract embedded media
- **Arrow IPC**: Export to Apache Arrow format for data processing
- **Fast**: Memory-mapped files and streaming decompression
- **Safe**: Rust's memory safety guarantees

## Installation

```bash
pnpm add pptx-fast-node
```

## Quick Start

```javascript
const { parsePptx } = require('pptx-fast-node');

const presentation = parsePptx('/path/to/slides.pptx');

console.log(`Title: ${presentation.metadata.core.title}`);
console.log(`Slides: ${presentation.slides.length}`);

for (const slide of presentation.slides) {
  console.log(`\nSlide ${slide.id}: ${slide.name || 'Untitled'}`);
  
  // Text shapes
  for (const shape of slide.shapes) {
    console.log(`  Shape: ${shape.name} (${shape.placeholder || 'custom'})`);
    for (const para of shape.paragraphs) {
      const formatting = [];
      if (para.runs.some(r => r.bold)) formatting.push('bold');
      if (para.runs.some(r => r.italic)) formatting.push('italic');
      console.log(`    ${para.text} ${formatting.length ? `[${formatting.join(', ')}]` : ''}`);
    }
  }
  
  // Tables
  for (const table of slide.tables) {
    console.log('  Table:');
    for (const row of table.rows) {
      console.log('    ' + row.cells.map(c => c.text).join(' | '));
    }
  }
  
  // Speaker notes
  for (const note of slide.notes) {
    const noteText = note.paragraphs.map(p => p.text).join(' ');
    console.log(`  Notes: ${noteText}`);
  }
}
```

## Options

```javascript
const presentation = parsePptx('/path/to/slides.pptx', {
  maxInflate: 128 * 1024 * 1024,  // Max decompressed size in bytes (default: 128 MiB)
});
```

## NDJSON Streaming

```javascript
const { parsePptxNdjson } = require('pptx-fast-node');

parsePptxNdjson('/path/to/deck.pptx', '/tmp/out.ndjson');
```

Output format (one JSON object per line):
```json
{"kind":"metadata","metadata":{"core":{"title":"My Deck"}}}
{"kind":"slide","slide":{"id":1,"name":"Title Slide","shapes":[...]}}
{"kind":"comment","comment":{"id":1,"slide_id":1,"text":"Great slide!"}}
```

## Arrow IPC Export

Export presentation data to Apache Arrow format:

```javascript
const { parsePptxArrowDataset } = require('pptx-fast-node');

const files = parsePptxArrowDataset('/path/to/deck.pptx', '/tmp/arrow-output');
console.log('Created:', files);
```

## Image Extraction

```javascript
const { extractPptxImages } = require('pptx-fast-node');

const imagePaths = extractPptxImages('/path/to/slides.pptx', './extracted-images');
console.log('Extracted images:', imagePaths);
// Output: ['./extracted-images/image1.png', './extracted-images/image2.jpg', ...]
```

## TypeScript Support

TypeScript definitions are included:

```typescript
import { parsePptx, PptxDocument, Slide, ShapeText } from 'pptx-fast-node';

const presentation: PptxDocument = parsePptx('slides.pptx');
```

## Document Structure

```
PptxDocument
├── metadata (core, app, custom properties)
├── slides
│   ├── shapes (text boxes, titles, content)
│   │   └── paragraphs → runs → text
│   ├── tables (rows → cells → text)
│   └── notes (speaker notes)
├── masters (slide masters)
├── layouts (slide layouts)
├── comments
└── images (embedded media)
```

## Performance

- **10x faster** than python-pptx
- **Low memory footprint**: Streaming decompression for large presentations
- **Parallel-ready**: Stateless parsing allows concurrent processing

## Platform Support

Prebuilt binaries available for:
- macOS (Intel & Apple Silicon)
- Linux (x64, ARM64)
- Windows (x64)

## License

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.

## Related Packages

- `xlsx-fast-node` - Excel (.xlsx) parser
- `docx-fast-node` - Word (.docx) parser
