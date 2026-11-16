# Email Fast

`email-fast-py` reads EML, Outlook MSG, and mbox through `email-fast-core`.

```python
import email_fast

email = email_fast.parse_email_bytes(
    upload_bytes, format="eml", max_inflate=32 * 1024 * 1024,
    max_total_attachment_bytes=8 * 1024 * 1024,
)
paths = email_fast.extract_attachments("message.msg", "attachments")
email_fast.parse_mbox_ndjson("archive.mbox", "archive.ndjson")
```

## Limits and errors

All entry points accept `max_inflate`, `max_mime_depth`, `max_mime_parts`, and
`max_header_bytes` as keyword-only arguments, plus the existing per-attachment
and aggregate attachment limits. The byte API forwards every limit to Rust.

Defaults are 128 MiB input/decoded content, 100 MiB per attachment and in total,
64 nesting levels, 10,000 MIME parts/MSG entries, and 256 KiB headers per MIME
part. MIME containers additionally have a hard preflight nesting cap of 32.
Input and decoded-content ceilings apply separately; these are not a
process-wide memory cap. Attachment sizes count decoded payload bytes rather
than MIME headers or Base64 overhead. Exceeded limits raise `RuntimeError`.

Mbox preserves original MIME bytes and removes one level of mboxrd `>From`
escaping. Parsing stops on the first invalid message with its index and byte
offset; it no longer silently omits failed messages. NDJSON or previously
completed extraction files may remain after an error.

## Output and compatibility

Parsing returns dictionaries; extraction writes attachments and returns their
actual paths. Sender-supplied directories are discarded, unsafe filename
characters are replaced, and collisions receive numeric suffixes. Existing
files and symlinks are not overwritten. EML payloads stream through decoding
into output files without an intermediate Base64 representation.

MIME alternatives prefer later supported representations; related resources
are distinguished from their root body. Address groups and quoted display names
use structured parsing. `message_id` appears once in JSON.

MSG reads recipient objects (To/CC/BCC), timestamps, threading, SMTP reply entry
IDs, message codepages, and compressed RTF text. Embedded messages are exported
as independently readable `.msg` attachments. See
[compatibility and specification notes](../SPEC_COMPLIANCE.md) for limits.

`parse_mbox_arrow` writes Arrow IPC files, splitting them by `batch_size` as
before. Existing Arrow schemas are unchanged. A batch size of zero is an error.

## Verify

Build/install the wheel, then run:

```sh
python -m pytest email-fast-py/tests -q
```

Licensed under either of MIT ([LICENSE](LICENSE)) or Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) at your option.
## An mbox mailbox from bytes

```python
import email_fast

mailbox = (
    b"From sender@example.com Mon Sep 21 00:00:00 2026\n"
    b"From: sender@example.com\nTo: recipient@example.com\nSubject: first\n\nbody\n"
)
messages = email_fast.parse_mbox_bytes(mailbox)
print([m["subject"] for m in messages])   # ['first']
```

`parse_mbox_bytes` takes the same keyword options as `parse_email_bytes`
and returns one dict per message, in mailbox order. A message that does not
parse is skipped.
