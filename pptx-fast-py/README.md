# PPTX Fast

`pptx-fast-py` is the read-only Python binding for deterministic PPTX
(presentation) extraction. It is a generic parser: the same call produces the
same presentation projection in every consumer.

## Contract

- **Input:** presentation bytes (in-memory) or a presentation path, with an
  explicit decompression budget.
- **Output:** a structured document dictionary; or NDJSON, Arrow IPC, or a
  directory of Arrow tables written to paths the caller names. The binding does
  not modify the input.
- **Streaming:** `parse_pptx_ndjson` writes one record per document element;
  `parse_pptx_arrow_dataset` writes one Arrow IPC file per shredded table. Both
  exist for presentations too large to hold in memory. `extract_pptx_images`
  writes embedded media to a directory.
- **Budget:** `max_inflate` applies to parsing **and** to image extraction. A
  caller that bounds the parse must be able to bound the extraction, or the
  extraction becomes the way around it.
- **Authority:** every result is unsigned. Successful parsing proves only that
  the accepted bytes were structurally readable as a presentation; it does not
  establish source ownership, slide meaning, or business interpretation.

Malformed archives, unsafe part paths, and exceeded expansion limits fail
loudly instead of yielding a partially correct presentation. A part the package
names but cannot be read is an error, not an omission: a silently skipped image
returns a list that looks complete. Callers must preserve the source hash and
treat extracted content as unsigned until a separate validation and promotion
decision.

## Install

```bash
pip install pptx_fast==0.2.1 --hash=sha256:<from dist/release-manifest.json>
```

Pin the version and verify the hash from `dist/release-manifest.json`. Every
wheel carries exactly one CycloneDX SBOM describing the crate that defines its
dependencies.

Licensed under MIT.
