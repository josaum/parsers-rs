# email-fast-node

High-performance email parser for .eml and .msg formats.

## Features

- **Fast**: Memory-mapped files for large mailboxes
- **Multi-format**: EML (RFC 5322) and MSG (Outlook) support
- **Projection**: Headers, body (text/html), attachments, threading
- **Streaming**: Mbox format support for bulk processing
- **Arrow IPC**: Export to Apache Arrow format
- **Safe**: Rust memory safety guarantees

## Installation

```bash
pnpm add email-fast-node
```

## Quick Start

```javascript
const { parseEmail, extractAttachments } = require('email-fast-node');

// Parse an email file (auto-detects format)
const email = parseEmail('/path/to/message.eml');

console.log('From:', email.from.address);
console.log('Subject:', email.subject);
console.log('Date:', email.date);

// Access body
if (email.body.text) {
  console.log('Text:', email.body.text.substring(0, 200));
}
if (email.body.html) {
  console.log('HTML:', email.body.html.substring(0, 200));
}

// List attachments
for (const attachment of email.attachments) {
  console.log(`Attachment: ${attachment.filename} (${attachment.size} bytes)`);
}

// Extract attachments to directory
const paths = extractAttachments('/path/to/message.eml', './attachments');
console.log('Extracted:', paths);
```

## Options

```javascript
const email = parseEmail('/path/to/message.eml', {
  maxInflate: 128 * 1024 * 1024,    // Input and decoded-content ceiling
  maxAttachmentSize: 100 * 1024 * 1024,
  maxTotalAttachmentBytes: 100 * 1024 * 1024,
  maxMimeDepth: 64,
  maxMimeParts: 10000,
  maxHeaderBytes: 256 * 1024,
  includeRawHeaders: false,        // Include all headers (default: false)
  htmlToText: false,               // Convert HTML to text (default: false)
  extractAttachmentContent: false, // Include attachment content (default: false)
});
```

## Mbox Processing

Process mailbox files with streaming:

```javascript
const { parseMboxNdjson, parseMboxArrow } = require('email-fast-node');

// Stream to NDJSON
const count = parseMboxNdjson(
  '/path/to/mailbox.mbox',
  '/tmp/output.ndjson'
);
console.log(`Processed ${count} emails`);

// Export to Apache Arrow
const count = parseMboxArrow(
  '/path/to/mailbox.mbox',
  '/tmp/emails.arrow',
  { batchSize: 10000 }
);
```

## Data Structure

```javascript
{
  message_id: "<abc123@example.com>",
  from: {
    name: "John Doe",
    address: "john@example.com"
  },
  reply_to: { name: null, address: "reply@example.com" },
  to: [{ name: null, address: "jane@example.com" }],
  cc: [],
  bcc: [],
  subject: "Hello World",
  date: "2024-01-15T14:30:00.000Z",
  body: {
    text: "Plain text content...",
    html: "<html>...</html>"
  },
  attachments: [
    {
      filename: "document.pdf",
      content_type: "application/pdf",
      size: 12345,
      content_id: "<attachment1>",
      is_inline: false
    }
  ],
  in_reply_to: "<parent@example.com>",
  references: ["<ancestor@example.com>"],
  priority: 3
}
```

## TypeScript Support

TypeScript definitions are included:

```typescript
import { parseEmail, Email, ParseOptions } from 'email-fast-node';

const email: Email = parseEmail('message.eml');
```

## Reliability and resource limits

All limits also accept snake_case names. `maxInflate` caps input and aggregate
decoded content separately; it is not a process memory cap. Attachment budgets
count decoded payload bytes. `extractAttachments(path, directory, options)`
accepts the same resource limits; its historical numeric third argument remains
supported.

Extraction discards sender-supplied directories and uses exclusive file creation.
Existing files/symlinks are not overwritten; collisions get numeric suffixes.
EML attachments decode directly into files, without Base64 re-encoding.

Mbox preserves original charset bytes and stops on parse errors, reporting the
message index and byte offset. Earlier output may remain after a failure. MIME
alternatives/related roots and quoted addresses are interpreted structurally.
MSG includes recipient objects, dates, threading, codepages, compressed RTF text,
and embedded `.msg` attachments.

See [compatibility notes and benchmarks](../SPEC_COMPLIANCE.md). Performance
figures describe measured workloads, not comparisons with other parser products.

## Platform Support

Prebuilt binaries available for:
- macOS (Intel & Apple Silicon)
- Linux (x64, ARM64)
- Windows (x64)

## License

MIT

## Related Packages

- `xlsx-fast-node` - Excel (.xlsx) parser
- `docx-fast-node` - Word (.docx) parser
- `pptx-fast-node` - PowerPoint (.pptx) parser
