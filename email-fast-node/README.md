# email-fast-node

High-performance email parser for .eml and .msg formats.

## Features

- **Fast**: Memory-mapped files for large mailboxes
- **Multi-format**: EML (RFC 5322) and MSG (Outlook) support
- **Complete**: Headers, body (text/html), attachments, threading
- **Streaming**: Mbox format support for bulk processing
- **Arrow IPC**: Export to Apache Arrow format
- **Safe**: Rust memory safety guarantees

## Installation

```bash
pnpm add email-fast-node  # or the @mycelia/email-fast-node consumer shell
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
  maxInflate: 100 * 1024 * 1024,  // Max attachment size (default: 100 MB)
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
  messageId: "<abc123@example.com>",
  from: {
    name: "John Doe",
    address: "john@example.com"
  },
  replyTo: { name: null, address: "reply@example.com" },
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
      contentType: "application/pdf",
      size: 12345,
      contentId: "<attachment1>",
      isInline: false
    }
  ],
  inReplyTo: "<parent@example.com>",
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

## Performance

- **10x faster** than JavaScript email parsers
- **Streaming mbox processing** for mailboxes with millions of messages
- **Low memory footprint** even for large attachments

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
