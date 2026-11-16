# web-fast-core

Deterministic extraction from static HTML, with JSON-serializable results and
optional Apache Arrow output. The caller supplies downloaded bytes, decoded
text, or a local file. This crate does not fetch URLs, execute JavaScript, or
load stylesheets and subresources.

```rust
use web_fast_core::{parse_html_bytes, ParseOptions};

let document = parse_html_bytes(
    b"<h1>Guide</h1><p>Read <a href='/next'>more</a>.</p>",
    Some("https://example.com/docs/"),
    ParseOptions::default(),
)?;
assert_eq!(document.text, "Guide\nRead more.");
assert_eq!(document.links[0].resolved_url.as_deref(), Some("https://example.com/next"));
# Ok::<(), web_fast_core::WebParseError>(())
```

`parse_html`, `parse_html_bytes`, and `parse_html_file` return `WebDocument`:
source SHA-256, effective base, title, language, description, canonical URL,
body text, semantic blocks, links, images, tables, JSON-LD, metadata, application
JSON (`json_data`), head discovery links (`discovery_links`), and payload-free
diagnostics. Occurrences and structural paths follow DOM order after HTML5
error recovery. The source digest always covers the exact original bytes.

## Extraction behavior

- Inline markup preserves word boundaries and punctuation: `hel<b>lo</b>!`
  becomes `hello!`. ASCII whitespace collapses; nonbreaking spaces remain.
- Body text includes text outside semantic blocks and separates block elements
  and `<br>` with newlines. Each text node appears once in `document.text`.
  Individual blocks retain their own subtree context, so nested blocks may overlap.
- `<pre>` keeps whitespace and line breaks. Hidden/`aria-hidden="true"` content,
  scripts, styles, templates, noscript, SVG, and canvas are excluded from visible
  text and resource lists. Template metadata is inert. CSS visibility and browser
  layout are not evaluated; this is not an `innerText` rendering engine.
- Tables retain empty cells and empty rows. Nested tables are extracted separately;
  their captions, rows, cells, and text do not enter their parent table's matrix.
  Cells are in source order; `rowspan` and `colspan` are not expanded into a grid.
- Description names, canonical relation tokens, and JSON-LD media types are
  matched case-insensitively, including MIME parameters. Invalid JSON-LD
  contributes a count to diagnostics.
- Repeated meta properties are retained. `application/json` scripts and strict
  JSON assignments to common hydration globals are captured with structural
  paths; arbitrary JavaScript expressions are not evaluated. Head `rel=next`
  and alternate JSON links are retained separately from body links.
- The first non-template `<base href>` controls relative URL resolution.
  Invalid, `data:`, or `javascript:` declared bases fall back to the supplied URL.
  Empty link hrefs resolve to the effective base. Raw attributes are preserved.
  Link targets are evidence, not sanitized or approved navigation destinations.

## Decoding and limits

Byte input uses an explicit WHATWG encoding label when supplied, otherwise a BOM,
then a valid meta declaration in the first 1,024 bytes, then strict UTF-8. Meta
inspection uses the HTML parser to exclude comments, quoted attributes, raw-text
and template content. Legacy `content` declarations require
`http-equiv="content-type"`. Meta UTF-16 labels map to UTF-8 and `x-user-defined`
maps to Windows-1252. Unknown explicit labels, conflicting BOM overrides, and
lossy decoding fail. This deterministic policy does not implement browser locale
fallbacks, XML declaration sniffing, or reparsing after later declarations.

| Option | Default | Scope |
| --- | ---: | --- |
| `max_input_bytes` | 32 MiB | Original source; checked before decoding/DOM parsing. File reads are also bounded. |
| `max_dom_nodes` | 1,000,000 | Parsed DOM nodes, including hidden content, comments, and text. |
| `max_dom_depth` | 256 | Element depth, including implicit HTML and body elements. |
| `max_nodes` | 250,000 | Emitted blocks, links, images, tables, rows, cells, and attempted nonempty JSON-LD values. |
| `max_text_bytes` | 32 MiB | Aggregate dynamic output strings, including metadata, URLs, paths, repeated text projections, and JSON-LD source. |
| `encoding_label` | `None` | Optional explicit strict decoding override. |

DOM limits are checked **after HTML5 DOM construction**, before extraction and
path generation. The original bytes, decoded text, DOM, indexes, JSON value
containers, and output coexist; these limits are not a process-memory cap.
The digest and fixed/count-only diagnostics are excluded from the text budget.

This release adds `metadata`, `json_data`, and `discovery_links` to `WebDocument`,
and `max_dom_nodes` and `max_dom_depth` to Rust `ParseOptions`;
use `..ParseOptions::default()` in struct literals. Older serialized options
continue to work through Serde defaults. Text limits now cover previously
uncounted attributes and output copies, and table rows/cells count as nodes.

## Arrow

Enable the `arrow` feature. `web_blocks_to_record_batch` explicitly materializes
all blocks in one batch. `write_web_blocks_ipc` writes an IPC **stream** in batches
of at most 1,024 rows; `write_web_blocks_ipc_batched` accepts a custom nonzero
batch size. Global ordinals and the existing schema are preserved. Empty results
emit one empty batch. Write and flush failures propagate; failed output can be
partial. Batching bounds additional Arrow rows, not the already-parsed document
or bytes per row. Extraction is DOM-based and allocates owned output strings.

## Verification

```sh
cargo test -p web-fast-core --locked
cargo test -p web-fast-core --features arrow --locked
cargo clippy -p web-fast-core --all-targets --features arrow --locked -- -D warnings
cargo bench -p web-fast-core --bench html_paths --locked
```

`tests/reliability.rs` covers text, nested/empty tables, charset and BOM edge
cases, template isolation, URL bases, file limits, and output/DOM budgets.
`tests/arrow.rs` covers batching, empty streams, schema/order, and writer errors.
See [SPEC_COMPLIANCE.md](../SPEC_COMPLIANCE.md) for standards and measurements.
For fetching URLs, executing JavaScript, API JSON capture, commerce normalization,
and indexed semantic graphs, use [web-fast-crawl](../web-fast-crawl/README.md).
