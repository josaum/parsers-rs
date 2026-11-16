# Parser compatibility and conformance checks

Reviewed against the published specifications on 2026-11-15; crawler/browser additions checked on 2026-11-16. This records the
scope of the JSON, XML, GeoJSON, email, shared ZIP and binary Excel changes, not full
certification of every format in the workspace. Arrow wire pins are unchanged.
Binary Excel uses Calamine 0.38.0 (released 2026-11-02); its dependency subtree
and the lockfile have been updated.

| Specification | Behavior covered | Regression evidence |
| --- | --- | --- |
| [RFC 8259, section 2](https://www.rfc-editor.org/rfc/rfc8259#section-2) | JSONL trims only JSON whitespace; invalid Unicode whitespace remains a parse error. Blank JSON-whitespace lines remain intentionally supported. | `json-fast-core/tests/spec_regressions.rs` |
| [JSON-RPC 2.0, sections 4–5](https://www.json-rpc.org/specification) | Object envelopes, structured parameters, string/number/null IDs, required response ID, exactly one result/error, object errors, and explicit null preservation. An absent request ID denotes a notification. Empty string methods are strings and are accepted. | `json-fast-core/tests/spec_regressions.rs`, `json-fast-py/tests/test_json_fast.py`, `scripts/smoke-node.mjs json` |
| [JSON:API 1.1, document structure](https://jsonapi.org/format/#document-structure) | Explicit `data: null`, mutual exclusion of data/errors, core member shapes, object metadata, and `included` requiring data. | Same protocol regression tests |
| [JSON Schema 2020-12, instance equality](https://json-schema.org/draft/2020-12/json-schema-core#section-4.2.2) and [validation](https://json-schema.org/draft/2020-12/json-schema-validation) | `uniqueItems` ignores object key order recursively; integer/float equality does not round large integers. Local anchors resolve inside schema applicator arrays and do not come from instance data such as `const`. Local JSON pointers check escapes and array index syntax. | `json-fast-core/tests/spec_regressions.rs`, schema unit tests |
| [PKWARE APPNOTE 6.3.10](https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT), sections 4.1.5 and 4.4 | Stored and Deflate parts verify uncompressed size and CRC32, including zero CRC, before either read API returns bytes. Duplicate normalized ZIP part names are rejected to avoid ambiguous OOXML part lookup. | `ooxml-archive/tests/integrity.rs`, `ooxml-archive/tests/proptest_invariants.rs` |

| [XML 1.0, well-formed documents](https://www.w3.org/TR/REC-xml/#sec-well-formed), [XSD 1.1 structures](https://www.w3.org/TR/xmlschema11-1/) and [datatypes](https://www.w3.org/TR/xmlschema11-2/#rf-whiteSpace) | One root, no non-whitespace text outside it, empty-element validation, CDATA/simple content, entity errors, minimum occurrences and built-in whitespace behavior. Error counts respect the configured positive cap. | `xsd-fast-core/tests/validation_regressions.rs` |
| [RFC 7946, sections 3.1 and 3.2](https://www.rfc-editor.org/rfc/rfc7946.html) | String ID fidelity, feature/property shapes, numeric finite ordinates, minimum line/ring sizes and ring closure across all ordinates. Either ring winding is accepted. | `geo-fast-core/tests/geojson_regressions.rs` |
| [MS-XLS 12.2 (2025-08-19)](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/cd03cb5f-ca02-4934-a391-bb674cb8aa06), [MS-XLSB 22.0 (2026-10-16)](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/acc8aa92-1f02-4167-99f5-84f9f676b95a) | Calamine binary readers project worksheet cells into the existing workbook/NDJSON/Arrow contract. Fixtures cover content detection, path/byte parity, hidden worksheets, order, dates in both epochs, formulas, encryption and truncation errors, and resource limits. | `xlsx-fast-core/tests/binary_workbooks.rs`, binary unit tests, `xlsx-fast-py/tests/test_binary_workbooks.py`, `scripts/smoke-node.mjs xlsx` |

## Compatibility and limits

Explicit null members now deserialize to `Some(Value::Null)` rather than `None`
in JSON protocol structs. Python and Node results preserve the corresponding
null-valued keys. Inputs that violated the checked envelope rules, and corrupt
ZIP parts previously accepted, now return errors. Protocol validation is provided
by each type's `parse` method; deriving `Deserialize` is not a substitute for it.

JSON:API resource contents, links, profiles and extensions are not fully validated
or retained by this envelope API. JSON-RPC helpers handle individual messages,
not batch dispatch. JSON Schema still reports warnings for unsupported constructs,
including remote references and dynamic/recursive references; schema dialects,
embedded resource scope, and arbitrary-precision numbers are not fully implemented.
These changes do not claim complete JSON Schema conformance.

The shared ZIP reader still borrows stored bytes and enforces the aggregate inflate
budget. Integrity checking adds a checksum pass over returned bytes. No OOXML
throughput improvement is claimed by this change.

### XML and GeoJSON limits

The XSD validator remains a subset: these fixes do not add complete namespace,
attribute, compositor, identity-constraint or XSD 1.1 assertion support. Unknown
types referenced by matched declarations now produce errors. `max_errors=0` is
rejected. Existing facets and supported structures retain their prior scope.

GeoJSON remains a two-dimensional projection: extra ordinates are validated but
not retained, and numeric feature IDs become strings internally. Null geometry
and GeometryCollection are not supported by this geometry model. This pass
strengthens parsing; it does not certify every serializer input.

### Binary Excel compatibility

The Rust `parse_excel*`, Python `parse_excel*`, and Node `parseExcel*` aliases
accept XLSX, XLS and XLSB. Existing `parse_xlsx*`/`parseXlsx*` calls also detect all
three from contents. Python and Rust support byte input. Flight lists `.xls` and
`.xlsb` alongside existing OOXML extensions and uses the same Arrow schema,
filters, authentication and path authorization.

XLS means compound-file BIFF workbooks; raw historical BIFF streams are not a
supported input. Binary worksheets are materialized before NDJSON/Arrow output;
parallel sheet workers remain XLSX-only. Hidden worksheets are included, chart
tabs skipped, and formulas/macros are never executed. Formula text is opt-in;
unsupported formulas can fail when requested. Cached values are not recalculated.
Encrypted workbooks are rejected. Binary raw strings are reconstructed from
values, not original XML lexical text. Duration-formatted binary cells remain
numeric serials instead of being mistaken for calendar dates.

`max_inflate` bounds aggregate ZIP expansion (XLSX/XLSB), XLS input bytes and
estimated XLS dense-range allocations. XLS/XLSB also apply a separate retained-
cell estimate against this ceiling. XLSB record framing is checked before reader
buffer allocation, and malformed binary reader panics become parse errors.
These guards are not a process-wide peak-memory or CPU-time guarantee. Dense XLS
ranges may hit the budget even when few cells are populated. No throughput claim
is made for the new binary formats. Fixture origins, hashes and license are in
`xlsx-fast-core/tests/fixtures/README.md`.

The current Microsoft specifications are reference targets, not a claim that
Calamine implements every record or extension. The existing XLSX projection does
not newly implement all [MS-XLSX 30.0 extensions](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/2c5dee00-eff2-4b22-92b6-0738acd4475e).

### Email correctness, extraction and MSG

The email pass uses mailparse 0.18.0 and CFB 0.16.0. RTF text projection uses
rtf-parser 0.5.0 without its WASM feature; compressed RTF is decoded with explicit
size, dictionary, completion and CRC checks before text parsing. Unicode fallback
runs are normalized before the RTF dependency to preserve accents/surrogate pairs
and prevent unchecked fallback-count allocations. Raw non-UTF-8 RTF text must use
RTF character escapes. The Microsoft
compressed-rtf crate is a test-only independent fixture generator.

- [RFC 2183 section 2.3](https://www.rfc-editor.org/rfc/rfc2183.html#section-2.3):
  extracted filenames are terminal components, sanitized, and created exclusively.
  Existing files and symlinks are not overwritten; collisions receive numeric
  suffixes. Base64, quoted-printable and raw EML payloads stream to the destination
  without Base64 re-encoding. Completed files may remain on a later parse error.
- [RFC 5322 addresses](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.4) and
  [RFC 2047](https://www.rfc-editor.org/rfc/rfc2047.html): structured header parsing
  handles quoted names/groups, avoids decoding encoded words twice, and emits
  only one `message_id` JSON key. Raw-header output retains its existing decoded
  projection; it is not an archival copy of header bytes.
- [RFC 2046 section 5.1.4](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.4)
  and [RFC 2387](https://www.rfc-editor.org/rfc/rfc2387.html): alternatives retain
  the last supported representation of each text type, related roots follow
  `start`/first-part selection, and mixed text bodies are combined in order.
  Content-ID alone no longer converts a body into an attachment. Related root
  filenames do not hide its body. Attached messages remain attachments.
- Mbox processing preserves bytes until MIME charset decoding, reuses buffers,
  removes one level of mboxrd escaping, and reports message indices/byte offsets.
  Default parsing now stops on errors. Rust `parse_mbox_with_errors` supplies an
  explicit skip policy; output callback errors always propagate. Input framing
  and oversized buffers are fatal. Content-Length-based mboxcl variants are not
  implemented; the supported framing is mboxrd/mboxo.
- [MS-OXMSG](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxmsg/1a69e000-f391-4c03-9d43-32d5f554bca7)
  recipient and property streams supply To/CC/BCC, dates, importance, codepages,
  message IDs/references, and reply-to. SMTP One-Off reply EntryIDs are supported;
  directory EntryIDs need external directory resolution and use transport-header
  fallback. Display labels are not invented SMTP addresses. Embedded messages
  are rebuilt with the top-level property header and named-property mapping so
  their extracted `.msg` files can be reparsed independently.
- [MS-OXRTFCP](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxrtfcp/)
  compressed/uncompressed RTF yields a plain-text projection. This does not add
  full RTF rendering, every named MAPI property, directory lookup, or message
  decryption. Unsupported text codepages and corrupt data return errors.

`max_inflate` now caps input and aggregate decoded content separately, including
MSG stream/RTF expansion checks. New Rust fields `max_mime_depth` (64),
`max_mime_parts` (10,000) and `max_header_bytes` (256 KiB) bound structure. Input
and root headers are checked before MIME parsing; mailparse also has an internal
recursion cap, and the configured tree limits are checked before content decoding.
These are not process-wide peak-memory or CPU limits. Attachment budgets apply
to decoded payload bytes; MIME headers/Base64 overhead no longer cause false
per-attachment failures. MSG now fails on budget violations instead of silently
omitting data.

Rust callers using exhaustive `ParseOptions` literals must add the new fields
or use `..Default::default()`. Existing Python signatures remain compatible and
add keyword-only limits; byte and path entry points forward all limits. Node
supports camelCase and snake_case options; Flight exposes corresponding flags.
The Arrow wire schema is unchanged. Multi-file Arrow output now honors batch
size; batch size zero is rejected. The existing mbox Arrow file-splitting contract
is preserved, and batch size bounds message count rather than total bytes.

Regression evidence: `email-fast-core/tests/email_reliability.rs`,
`email-fast-core/tests/msg_fidelity.rs`, the RTF unit test,
`email-fast-py/tests/test_email_reliability.py`, and `scripts/smoke-node.mjs email`.
The six original reliability probes all failed before the changes and now pass.

## Static web extraction

`web-fast-core` now uses `scraper` 0.29.0 / `html5ever` 0.40.1 for HTML5 tree
construction, with unused CLI and parse-error collection features disabled.
It remains a pure static parser; retrieval, JavaScript, CSS rendering, and crawl
policy belong to the caller.

Relevant primary references are the [HTML Living Standard parsing model](https://html.spec.whatwg.org/multipage/parsing.html),
[encoding declarations](https://html.spec.whatwg.org/multipage/parsing.html#determining-the-character-encoding),
[base URL rules](https://html.spec.whatwg.org/multipage/semantics.html#the-base-element),
and [scraper 0.29.0 documentation](https://docs.rs/scraper/0.29.0/scraper/).
This is a documented static extraction policy, not complete browser conformance:

- Bounded, markup-aware meta detection ignores comments, unrelated attributes,
  raw text, and templates; legacy content declarations require the pragma.
  UTF-16 and x-user-defined meta labels receive the HTML encoding remappings.
  Strict explicit encoding overrides reject a conflicting BOM. Without a label,
  BOM or early declaration, strict UTF-8 is used; no locale or XML sniffing.
- Visible text preserves inline adjacency, punctuation, nonbreaking spaces, and
  preformatted whitespace. Body text traverses text nodes once, preserving
  unwrapped content without duplicating nested semantic blocks. Explicit hidden
  content and inert templates do not leak into text, resources, or metadata.
- Nested tables have separate row/cell ownership; empty cells retain their column
  positions. Table spans are not expanded, and CSS visibility is not evaluated.
- Metadata tokens are case-insensitive. The first non-template declared base
  resolves against the supplied URL; invalid/data/javascript bases fall back.
  Empty links are preserved and resolved. Link schemes are not sanitized.
- A single iterative index computes visibility, table ownership, and sibling
  positions. Paths no longer rescan earlier siblings, and extraction avoids
  repeatedly compiling selectors or allocating word vectors.
- Source reads are bounded even if a file grows after its metadata check. New
  `max_dom_nodes` and `max_dom_depth` limits are checked after DOM construction,
  before extraction. Aggregate output budgets include attributes, URLs, paths,
  repeated text projections and JSON-LD source; table rows/cells count as nodes.
  These are not process-memory limits, and DOM creation still materializes input.
- Arrow IPC streams retain the schema and global ordinals while defaulting to
  batches of 1,024 rows. An explicit batch API rejects zero; write and flush errors
  propagate. The one-record-batch API remains available for callers who need it.

Compatibility: exhaustive Rust `ParseOptions` literals need the two new fields
or `..Default::default()`; Serde fills them for older serialized options. Corrected
text and table extraction intentionally changes results for affected edge cases,
resource budgets are stricter, and large IPC streams now contain multiple batches.
See `web-fast-core/README.md` for the precise scope and defaults.

Regression evidence: `web-fast-core/tests/reliability.rs` and
`web-fast-core/tests/arrow.rs`. Nine initial correctness probes failed before the
changes; the full suite adds structural limits, BOM/charset boundaries, inert
template metadata, exact budget boundaries, and Arrow failure/batching coverage.

## Semantic crawler and modern websites

`web-fast-crawl` adds a network/browser boundary around the pure HTML parser.
The Rust API and CLI produce JSON reports, structured commerce observations, and
interned RDF graphs with outgoing/incoming/predicate CSR indexes. The existing
binding release matrix is unchanged.

| Reference | Implemented scope | Regression evidence |
| --- | --- | --- |
| [RFC 9309](https://www.rfc-editor.org/rfc/rfc9309.html) | Per-origin robots checks/cache, redirect handling, 512 KiB file cap, unavailable-file policy; 401/403/429 are conservatively denied. | `web-fast-crawl/tests/crawler.rs` |
| [JSON-LD 1.1 processing](https://www.w3.org/TR/json-ld11-api/) | Offline context loading and RDF projection through Oxigraph; typed/language literals, lists, scoped blank nodes and named graphs. Unsupported directional/triple terms reject a script atomically. | `web-fast-crawl/tests/graph_semantics.rs` |
| [Schema.org JSON-LD context](https://schema.org/docs/jsonldcontext.jsonld) and [terms](https://schema.org/docs/terms.html) | Bundled context snapshot with CC BY-SA 3.0 attribution, product/offer observations and explicit source pointers; no automatic entity equivalence from canonical hints. | `web-fast-crawl/contexts/NOTICE.md`, `tests/structured.rs` |
| [Playwright request routing](https://playwright.dev/docs/network), [browser contexts](https://playwright.dev/docs/api/class-browsercontext) | Pinned Playwright 1.65.1, fresh browser context, Rust-controlled HTTP requests, bounded settling/scrolling, API JSON and open-shadow-root capture; service workers and WebSockets disabled. | `web-fast-crawl/tests/browser_e2e.rs`, `.github/workflows/ci.yml` |
| [Next.js hydration data](https://nextjs.org/docs/messages/large-page-data) | Application JSON scripts and known hydration globals are retained; arbitrary static JavaScript is not evaluated. Framework-specific streaming/devalue formats are raw payloads, not fully decoded models. | `web-fast-core/tests/reliability.rs`, `web-fast-crawl/tests/structured.rs` |

GraphQL POST operation documents are parsed before sending; only selected query
operations are allowed by default. Mutations/subscriptions are blocked. Unknown
persisted hashes request a full document through a synthetic not-found response.
Explicit REST POST opt-ins remain the caller's classification responsibility.

The browser navigation regression initially failed because a JavaScript destination's
`X-Robots-Tag` was lost; the renderer now aggregates navigation restrictions before
retaining page content. Its fixture verifies both noindex and nofollow. Browser IPC
also has a cancellation regression covering a partially read JSON protocol line.

A live Mercado Livre crawl exposed browser-valid tracking fragments with multiple
`#` characters that are invalid RDF IRIs. HTML link/canonical evidence now preserves
such URLs as literals while valid fragment-free targets remain resource edges.
Malformed fetch IRIs are rejected before admission. Graph insertion diagnostics
distinguish invalid operations from resource limits. The local-server regression
`browser_valid_tracking_fragments_do_not_abort_rdf_projection` reproduces the failure
without depending on live marketplace content.

Amazon detail extraction uses explicit buy-box amounts/currencies, separates resale
offers, supports both observed accordion and single-offer layouts, and labels
feature-bullet descriptions as bounded excerpts. `amazon_html.rs` regression
coverage checks price isolation and script-free stock text. The crawler HTTP test
`waf_challenge_is_not_successful_content_or_retried` covers HTTP 202 access
challenges and fail-closed challenged robots responses. This is site-specific
HTML evidence, not an assertion of Amazon API or schema conformance.

`web-fast-crawl/examples/olaya-products.mjs` optionally runs the local O-Laya Node
lexical classifier against a custom ontology. Source facts, hashes, extraction
evidence, and missing fields remain distinct from inferred classes and ancestors.
The adjacent integration test executes the actual runtime when `OLAYA_RUNTIME` is
set. Consistency and confidence are not calibrated accuracy or independent fact
verification; the example taxonomy is not GS1 certification.

Mercado Livre product validation now includes a bounded HTML-card adapter for
current prices, explicit BRL currency names, and payment context. Original prices
and installments do not become the current price. Card observations retain DOM
selector/ordinal provenance and do not fabricate missing full descriptions or stock.
Fixtures in `commerce_html.rs` cover this distinction and inert templates. Structured
JSON regression tests cover descriptions, conditions, seller IDs, currency aliases,
images, and zero stock. A redirect-to-verification fixture proves that the crawler
reports the blocked redirect destination separately from the requested product URL.

User-Agent selection is now independent of the internal robots token. Browser-mode
requests use the actual selected browser identity (including JavaScript and bridge
requests); HTTP-only mode omits the header unless configured. `--headed` uses a
visible browser, and explicit values are validated against header injection. HTTP
fixtures and the Chromium integration test assert identity parity across robots,
initial document, API requests, and `navigator.userAgent`.

Graph lookup benchmark (`cargo bench -p web-fast-crawl --bench graph_paths --locked`):
a 100,000-edge chain, same single-neighbor query and result consumption, release
build on Apple Silicon: full scan 22,414.5 ns/query versus indexed 17.2 ns/query.
This measures adjacency lookup only, including harness overhead; it is not a network
or browser throughput claim.

See [crawler documentation](web-fast-crawl/README.md) for resource limits, deployment,
price-unit semantics, source hashing, and unsupported features. No complete framework,
marketplace, browser-network isolation, or RDF conformance claim is made.

## Performance evidence

Run `cargo bench -p json-fast-core --bench parse_paths --locked`. The dependency-free
harness warms up each workload, then reports the median of seven timed samples.
Compare on the same machine with other builds idle; timings are not CI assertions.

Local Apple Silicon, Rust 1.98.1, release profile; baseline source `ae5af91`:

| Workload | Before | After | Throughput change |
| --- | ---: | ---: | ---: |
| Query a 50,000-integer array into `Vec<u64>` | 1,020.1 µs | 865.1 µs | +18% |
| Parse 10,000 JSONL objects with CRLF and surrounding spaces | 2,024.5 µs | 1,760.8 µs | +15% |
| Validate XML with 1,000 rows × 20 string fields | 7,915.0 µs | 2,916.5 µs | 2.71× |
| Parse a small EML with encoded subject, addresses, date and plain body | 26.675 µs | 2.206 µs | 12.09× |
| Extract HTML with 10 paragraph/link pairs | 25.857 µs | 22.068 µs | 1.17× |
| Extract HTML with 1,000 paragraph/link pairs | 5,101.458 µs | 1,943.554 µs | 2.62× |
| Extract HTML with 10,000 paragraph/link pairs | 652,566.792 µs | 19,916.765 µs | 32.76× |

Queries deserialize from the borrowed selected value without cloning its subtree.
JSONL preflight checks physical line limits during the token/budget pass and
returns the record count for a single output-vector allocation. Aggregate preflight still
finishes before yielding any parsed record. Schema anchor indexing also borrows
schema nodes instead of allocating a JSON pointer string for each traversed value.

The XML benchmark (`cargo bench -p xsd-fast-core --bench validate --locked`)
loads the schema before timing and uses the same seven-sample median. Validation
borrows schema types and content models, reads borrowed XML events, and avoids
cloning completed element text/count maps.

The EML benchmark (`cargo bench -p email-fast-core --bench email_paths --locked`)
uses seven 200 ms samples on the same input before and after this pass. Its
speedup primarily removes repeated regex compilation and duplicate header
decoding; it is not a claim about large attachments, MSG or other parsers.

The HTML benchmark (`cargo bench -p web-fast-core --bench html_paths --locked`)
uses five samples of at least 200 ms, on the same repeated paragraph/link input
before and after this pass, with other builds idle. The large-page improvement
primarily removes quadratic sibling scans; it is not a claim about network
scraping, JavaScript execution, or every HTML shape.

## Verification commands

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
# Also run the Arrow feature matrix documented in README.md.
cargo test --locked --manifest-path .leio-code/rust-doctors/Cargo.toml
cargo run --locked --manifest-path .leio-code/rust-doctors/Cargo.toml -- --check-root .
scripts/build-node.sh json-fast-node xlsx-fast-node email-fast-node
node scripts/smoke-node.mjs json xlsx email
# Against freshly built json-fast and xlsx-fast wheels, with pytest and pyarrow:
python -m pytest tests json-fast-py/tests xlsx-fast-py/tests email-fast-py/tests -q
```
