# Changelog

Changes a consumer of the wheels, Node packages, crates or Flight servers
can observe. Each release also has generated notes listing every pull
request.

## Unreleased

### Changed output

- **email:** mailparse moves from 0.15 to 0.18, which splits a message
  differently:
  - The line break before a boundary delimiter belongs to the delimiter
    (RFC 2046, section 5.1.1), so it is no longer left at the end of the
    part above it. A text or HTML body loses the CRLF (or LF) it used to end
    with, and an attachment loses those bytes from `content` and `size`.
  - A delimiter that ends the message, or ends a part, is found. 0.15 missed
    it and read it as text.
  - Every type that begins with `multipart` and declares a `boundary` is
    split into its parts, so is `multipart` with no subtype. 0.15 read such
    a part as one leaf.
  - A part is a container exactly when mailparse splits it. A part whose
    type begins with `multipart` but that declares no `boundary` is a leaf,
    and its content is kept as an attachment. Before, such a part was
    dropped when its type had a slash (`multipart/mixed`).
  - `Content-Type` and `Content-Disposition` parameters are split only at
    semicolons outside quotes, so a quoted value holding `;` is read whole.
    A backslash in a quoted value now escapes the next character, and RFC
    2231 continuations are decoded after joining, so a character split
    across two segments survives. These two can change a `filename`; neither
    occurs in the files measured below.
  - Address lists read comments in more places, and a `:` after an address
    that has an `@` no longer opens a group (`to`, `cc`, `from`).
  - mailparse refuses nesting past 100 levels. The crate's own limit of 32
    is checked first, so no message changes because of it.

  Measured against 0.15:
  - 26 real .eml files: 16 parse to different JSON, all because of the
    line break (`body` in 16, `attachments` in 2). Every changed body only
    lost the line break before a delimiter.
  - 11,925 distinct messages that email-fast-core's tests parse: 1,752
    differ. One of them is a mutated message whose nested part lost its
    `boundary` parameter and declares base64: it used to be dropped, it is
    now read as a leaf, and its body is not base64, so the message fails as
    any message with such an attachment does. The other 1,751 differ in
    `body` (1,515), `attachments` (1,513), `to` (15), `cc` (6) and `from`
    (2). Counted by field, 2,930 differences come from the line break
    alone, 48 from a boundaryless `multipart` part now kept (one more
    attachment each), 23 from address parsing (the ones inspected are
    mutated, malformed address lines), 4 from quoted semicolons, 1 from a
    delimiter at the end, and 45 from two or more of these together.
- **xsd (Arrow):** sibling columns no longer share a name. Before, a type
  whose elements share a local name (repeated in a sequence, in branches of
  a choice, inherited and redeclared, or in a nested group) gave a struct,
  or a top-level schema, with several children of that name, which pyarrow
  (`StructArray.field(name)`: `KeyError`) and polars (`DuplicateError`)
  could not read by name. Now they are one column, at the position of the
  first:
  - across the branches of a choice that occurs at most once, where each
    branch holds the name once and none repeats it, a scalar of their
    common type;
  - otherwise (a sequence, a repeated choice, a branch that repeats the
    name or holds it twice) a `List` of their common type. A repeated
    particle adds its items; any other list column (a child whose one
    column is a list, or an `xs:list` value) is one item, making a list of
    lists.

  A child two of whose own columns merged stays a struct. Elements whose
  types differ keep their own columns and positions and are renamed
  `name#2`, `name#3`, ... (none on the corpus). Attributes keep their `@`
  prefix, so an attribute and an element never collided.
- **xsd (Arrow):** a column under an `xs:choice` is nullable unless the
  choice is required and the column's name occurs in every branch. A
  type's own top-level choice was not covered: a branch element such as
  XML-DSig `PGPKeyID` in `PGPData` was non-nullable although the other
  branch leaves it out. A column of a nested choice that occurs in every
  branch is no longer forced nullable.
- **xsd (model):** a sequence that is one branch of a choice is kept as an
  `ElementRef::Group` holding a `ContentModel::Sequence`, even when it
  occurs once; it used to be written into the choice, so `(a, b) | (a, c)`
  read as one choice of four elements. Validation reads such a branch as it
  did before, so its findings do not change.

  Measured on 452 real schemas (399 load; the same ones load, and no
  element's derivation newly fails), counting each global element's derived
  tree separately: 196 schemas change. Groups of same-name siblings go from
  164,082 (528 distinct per struct shape and name) to 0. Of the 20,436
  merged columns, 1,201 are scalars, 1,280 keep their parts' list depth
  (repeated parts in one list, or a scalar of list values) and 17,955 add a
  list level; 181,105 column paths gain a list level. Column paths, counting
  each duplicate's subtree, fall from 7,862,594 to 4,199,285; the 4,199,285
  distinct paths are the same before and after (list levels ignored), so
  none is lost. Apart from the merges and their ancestors, no column changes
  type or order, and 301,480 change nullability (301,479 become nullable, 1
  non-nullable). In the source, 21 of 276 (choice, name) groups merge into
  a scalar; in 227 every part repeats (lists before too, now one list), 22
  have a branch that holds the name twice and 6 a repeated choice. The
  model changes for 241 schemas; written back into their choices, their
  branch sequences give exactly the earlier model for all 399, and that is
  what validation reads. Validation of 12 instance documents found beside 7
  of the schemas gives the same findings before and after.

### Fixed

- **email:** a message nested 100,000 multipart levels deep took 8 s to
  refuse in an optimized build, because the scan that bounds header
  decoding compared every body line that begins with `--` with every
  boundary declared before it. The same work let a message one level deep
  that declares many boundaries take seconds to accept. The scan now does
  one comparison per such line; that message is refused in about 0.4 s,
  mostly the nesting walk. What is refused and what is accepted, and with
  what result, is unchanged.

## v2026.11.13.2

v2026.11.13.1 was tagged but never released: its Linux x86_64 wheels did
not build (libdeflater 1.26 under the release's zig toolchain), so nothing
was published under it. This is that release with libdeflater back at
0.12, the version v2026.11.13 shipped with; the crate versions below are
the ones v2026.11.13.1 named.

Every crate's patch version moves, and `xsd-fast-core` moves to 0.3.0 with
its Python and Node packages, for a source-breaking change to its model
types (see "Public Rust API"). The headline is a memory-safety fix in the
`pdf_fast` Python module, JSON Schema `$id`/`$anchor`/`$dynamicRef`
resolution with unresolvable references failing closed, PDF 1.5+ sources in
pdf-write, named simple types and attribute groups in xsd, and quick-xml
0.42 without rejecting documents 0.41 read. Read "Changed output" and "New
refusals" before upgrading a pipeline that stores parsed output.

### Changed output

- **docx, xlsx, pptx, xsd, geo:** quick-xml moves from 0.41 to 0.42, which
  rejects a whole document on its first byte that is not UTF-8. Every
  reader now repairs its input first, replacing each such sequence with
  U+FFFD exactly as before, so documents read the same: such a byte reads
  as U+FFFD in text (and in docx attributes) and costs nothing where no
  value is read, a value that was read strictly (pptx and xlsx attributes,
  relationship `Id`/`Type`/`Target`, xsd `schemaLocation`) is still an
  error, and a name or value that xsd and SAR dropped is still dropped. A
  U+FFFD the document really holds is not mistaken for a repaired byte
  (but see Known gaps).
  On 524 real files (docx, xlsx, pptx, xsd; 77 of the xsd hold bytes that
  are not UTF-8, 2 docx hold UTF-16 parts) all 489 that parse give
  byte-identical output and the other 35 fail with the same error as
  before. Only error texts change,
  on paths that already failed: a strictly read attribute reports
  `attribute value is not UTF-8` and an xsd `schemaLocation` reports
  `invalid schemaLocation: not UTF-8`, instead of quick-xml's UTF-8
  decoding error.
- **json:** a `$ref` or `$dynamicRef` that resolves to nothing is now an
  error naming the reference, so the document is not accepted, even under
  `not` or beside a passing `anyOf` branch. It used to be a warning and was
  skipped, which accepted the value. Nothing is fetched, so a remote
  reference fails this way unless its document is supplied to the new
  `json_fast_core::schema::validate_with_resources`, or to the new optional
  `resources` argument of `json_fast.validate` (Python, keyword-only, a dict
  from URI to schema text) and of `validate` in `json-fast-node` (a trailing
  plain object from URI to schema text; anything else throws a `TypeError`).
  Both bindings take the documents in URI order, so when two claim one URI
  through `$id` the first by URI order wins, and a document that does not
  parse raises an error naming its URI. References to the JSON Schema
  meta-schemas (`json_fast_core::schema::WELL_KNOWN_META_SCHEMAS`) stay a
  warning and are skipped.
- **pdf (CCITT images):** image extraction decodes CCITTFax with
  hayro-ccitt 0.5. Several changes follow from it:
  - A row that truncated or damaged data cuts off partway is padded instead
    of keeping its partial pixels.
  - A damaged Group 3 row with end-of-line markers no longer borrows codes
    from the other colour. It is padded, and decoding resumes at the next
    row instead of stopping.
  - Under `/BlackIs1 true`, columns past `/Columns` and rows the stream does
    not code are padded with coded white (sample 0), not 0xFF, which is a
    coded black sample there.

  On 487 real CCITT images (Group 4 and Group 3 1D, none with `/BlackIs1
  true`) the output is identical.
- **json:** an `$id` or anchor inside `enum`, `const`, `default`,
  `examples` or an unknown keyword no longer identifies a schema. Two
  resources with one URI: the first in document order is used, with a
  warning. Under a draft-04/06/07 `$schema`, an object holding `$ref` sets
  no base URI and no anchor, except the document root, whose `$id` names
  the document in every dialect.
- **json:** validation stops with an error when resolving `$id`s and
  references needs more than 16 bytes of URI text per byte of schema text
  (plus 1 MiB), as a long base URI under many relative `$id`s would. A
  fragment-only reference (`#...`) is charged its fragment only.

- **pptx:** `docProps/app.xml` is read by the parser docx uses (#68).
  `HeadingPairs` and `TitlesOfParts`, and any property holding a vector,
  appear under their own names, one line per item; the keys `lpstr` and
  `i4` are gone. A property holding only white space with a line break is
  no longer stored. On 100 real `.pptx` files metadata changed in 89, all
  in `app`; nothing outside metadata changed.
- **xsd (Arrow):** named simple types are followed through restrictions,
  `xs:list` and `xs:union`, and `xs:attribute ref` / `xs:attributeGroup
  ref` contribute their attributes (#85). New columns appear for those
  attributes, text columns become numeric, boolean or temporal where the
  resolved type is, and `xs:list` becomes `List<item>`. On 452 real
  schemas column paths went from 3,231,555 to 8,261,655 (the attributes
  that were dropped), 82 columns changed from text to `Int64`/`Float64`,
  and none was removed. Referenced attributes on a child that otherwise
  gives one column are left out so no existing column changes shape;
  `ArrowShape::with_attributes_on_simple_children` includes them.
- **xsd (validation):** facets along simple-type chains and list items are
  checked; a choice whose matching branch is a wildcard (`xs:any`) is no
  longer reported as missing (#85).
- **Flight servers:** jsonwebtoken 11.1 accepts a correctly signed token
  whose header carries a non-string extra field (signature, algorithm,
  expiry, issuer and audience all still verify), and refuses an empty
  `aud` array (#79).

### Fixed

- **json:** `$id` base URIs (RFC 3986, including embedded resources),
  `$anchor` scoped to its resource, and `$dynamicRef`/`$dynamicAnchor` with
  the 2020-12 dynamic scope are resolved; `unevaluatedProperties` and
  `unevaluatedItems` see through `$dynamicRef`. The JSON Schema Test Suite's
  2020-12 core cases go from 1,239 to 1,299 of 1,300. This closes the json
  known gap listed under v2026.11.13.
- **pdf:** a panic inside the interpreted renderer reached the caller as a
  panic (Python `PanicException`, a Rust caller's thread unwinding). It is
  now an error, `interpreted render failed: the interpreter panicked on
  this page` (Python `RuntimeError`, Node `Error`), and
  `page_rasterized_png_interpreted` returns `None`. The interpreter's own
  panic message still goes to stderr through the panic hook.
- **geo:** `parse_geotiff_bands` (and `geotiff_bands` in Python and Node)
  returned wrong pixels for two kinds of raster instead of an error. For
  samples of 1, 2 or 4 bits it returned the packed bytes as values. For an
  uncompressed strip or tile storing fewer bytes than its pixels take,
  including a sparse block (offset and length zero), it filled the pixels
  from whatever followed in the file, for a sparse block the file's own
  header. Both are now refused (see "New refusals"). On GDAL's 237 test
  TIFFs, 3 rasters that returned such values are refused, and no other
  result changes.

- **pdf (Python), memory safety:** `pdf_fast.release_arrow_c_data` passed
  any two integers to `Box::from_raw`, so a made-up pair, or a pair
  released twice, corrupted the heap. Only a pair this module issued and has
  not freed is accepted now; anything else raises `ValueError`. The
  deprecated `*_c_data` import helper leaked two small structs per call; it
  frees them now (#70).
- **json bindings:** a Rust panic in `json-fast-node` is a thrown `Error`,
  not the end of the Node process; `json_fast` releases the GIL around every
  core call and installs on Python 3.8, and ships `py.typed` and a stub
  (#66). The ten binding pairs now behave alike; this closes the two "not
  yet" notes under v2026.11.13.
- **pdf-write:** sources with cross-reference streams, object streams,
  hybrid `/XRefStm` sections and `/Prev` chains through them (PDF 1.5 and
  later) extract instead of failing; 29 of 92 real PDFs in an earlier
  sample failed this way (#76). Output is still a classic table. This
  closes the pdf-write known gap listed under v2026.11.13.
- **geo (`http` feature):** WFS and `get_map` responses have size limits,
  and `parse_capabilities_xml_with_report` / `get_capabilities_with_report`
  report the WMS layers whose inherited reference systems were cut
  (#67). This closes the two geo known gaps listed under v2026.11.13.
- **Packaging:** every Python wheel names the repository, documentation and
  issue tracker in its metadata (`Project-URL`) (#84).
- **CI:** the geo `http`, json `mmap` and pdf `glyph-catalog` features are
  linted and tested (#65), closing the CI known gap listed under
  v2026.11.13.

### New refusals

- **docx, xlsx, pptx, xsd, geo:** a part or document that holds bytes that
  are not UTF-8 and outgrows its size limit only once each is replaced by
  U+FFFD (three bytes for each) is refused with `part is larger than N
  bytes once repaired to UTF-8`. The limit is `max_inflate` for the parts
  docx, xlsx and pptx stream (document body, headers and footers, sheets,
  shared strings, slides, notes) and `max_schema_bytes` for an xsd schema.
  Where no limit is known (parts read whole, relationship and property
  parts, xsd instance documents, SAR XML), input up to 128 MiB may not grow
  past 128 MiB, and larger input is not capped. An xsd instance refusal is
  a parse-error finding. Valid UTF-8 is never refused this way. With the
  defaults, a 120 MiB part of such bytes, which used to be read at up to
  1.3 GiB of memory, is now refused at about 260 MiB.
- **pdf:** the interpreted renderer refuses a tiling pattern that needs
  more than 10,000,000 copies of its cell to cover the page, or whose step
  is zero, infinite or NaN, or whose `/Matrix` collapses the cell. On page
  one of 1,295 real files the most any pattern needed was 98,929; none is
  refused.
- **geo:** a GeoTIFF raster whose samples are not a whole number of bytes
  (1, 2, 4, 10 or 12 bits), and an uncompressed raster with a strip or tile
  shorter than its pixels, including a sparse one.

- **pdf:** the interpreted renderer's guard refuses, before the interpreter
  runs (#78): more than 4,096 consecutive CMap comment lines; a sampled
  function over 2^20 samples or 8 inputs; more than 4,096 saved graphics
  states along one path; an image (XObject, soft mask, inline) over
  max(256 M, 16 × the page's pixels at the requested resolution); tiling
  cells, summed along a nesting path, over the same pixel budget. On 150
  real PDFs at four resolutions none is newly refused.
- **pdf-write:** decoding past 16 × the file length plus 64 KiB, more than
  one cross-reference entry per 8 bytes of file, more than 1,024 chained
  sections, a malformed `/W`, `/Index` or predictor, a hybrid file whose
  `/XRefStm` stream is corrupt, and a page that refers to a
  cross-reference or object-stream object (#76).
- **geo (`http` feature):** a WFS response over 256 MiB
  (`MAX_WFS_RESPONSE_BYTES`) or nested deeper than 32 levels; a `get_map`
  whose width × height × 4 is over 256 MiB is refused before connecting,
  and its response is held to width × height × 4 plus an eighth (#67).
- **pdf (Python):** `page_table_from_ocr_arrow_ipc` and its layout and
  `scan_*` forms raise `ValueError` naming the column when `text`, `px_x`,
  `px_y` or `px_height` is missing or mistyped (#78).

### Public Rust API

- **xsd-fast-core 0.3.0 (source-breaking):** `SimpleTypeDef.facets` is now
  `Arc<SimpleFacets>`, with new fields `list_item` and `union_members`;
  `AttributeDecl` gains `referenced`; `ElementRef` gains `Any(Wildcard)`;
  `ElementRef`, `AttributeDecl`, `SimpleTypeDef` and `Wildcard` are
  `#[non_exhaustive]`, so struct literals and exhaustive matches of them
  break. New: `ArrowShape::with_attributes_on_simple_children` and
  `to_arrow_schema_with_shape`. Models serialized by v2026.11.13 still
  load. `xsd-fast-py` and `xsd-fast-node` move to 0.3.0 with it; their own
  APIs are unchanged (#85).
- **json-fast-core:** new `schema::validate_with_resources` and
  `schema::WELL_KNOWN_META_SCHEMAS` (#77). `json_fast.validate` gains a
  keyword-only `resources`, and `validate` in `json-fast-node` a trailing
  object, both from URI to schema text.
- **geo-fast-core:** new `parse_capabilities_xml_with_report`,
  `WmsClient::get_capabilities_with_report`, `WmsCapabilities` and
  `LayerRef` (both `#[non_exhaustive]`), and the constants
  `MAX_WFS_RESPONSE_BYTES`, `MAX_MAP_RESPONSE_BYTES` and
  `MAP_RESPONSE_FLOOR_BYTES` (#67).
- **ooxml-archive:** new `properties::parse_simple_props`, shared by docx
  and pptx (#68). `xsd-fast-core` and `geo-fast-core` now depend on
  `ooxml-archive` for its UTF-8 repair reader (#92).
- Checked with cargo-semver-checks against v2026.11.13: every other library
  crate needs no more than its patch release; the Flight servers have no
  library API.

### Known gaps

- **docx, xlsx, pptx, xsd, geo:** a reader remembers where it replaced
  bytes that are not UTF-8 in up to 4,096 separate runs per part. Past
  that, the runs merge into one, and a strictly read value inside the
  merged run that holds a real U+FFFD is treated as not UTF-8.
- **pdf:** still open from v2026.11.13: three documented cases where text
  extraction and the renderer read different copies of an object in a
  malformed file, and a glyph procedure re-read for every code shown. The
  render guard cannot read the page tree as hayro does without an upstream
  API, has not traced the image decoders' internals (JPEG, JPX, JBIG2,
  CCITT), and does not bound the clip paths each saved state copies.
- **Dependencies held** (`.github/dependabot.yml` gives each reason):
  hayro 0.6 and later, tiff 0.10 and later, libdeflater 1 and later,
  mailparse 0.16 and later, napi
  3, tiny-skia 0.12 and Arrow 59 and later.

## v2026.11.13

Every crate's patch version moves. The headline is hardening against
malformed and hostile input across all parsers, plus several long-standing
correctness bugs. Read "Changed output" and "New refusals" before upgrading
a pipeline that stores parsed output.

### Fixed

- **pdf:** an incrementally saved PDF was read as its oldest revision: page
  count, text, tables and rendering all came from the first catalog in the
  file. The newest revision is read now (#58).
- **pdf:** a Type 3 font glyph that shows itself aborted the process with a
  stack overflow (#59).
- **json:** `uniqueItems` was applied to every array, so any array with a
  repeated value failed any schema that reached it (#45).
- **Node bindings:** a Rust panic ended the Node process. In nine of the
  ten packages it is now a thrown error (`json-fast-node` not yet). Two
  functions of `pdf-fast-write-node` were declared but not exported (#55).
- **Python bindings:** pip refused every wheel on Python 3.8 although the
  wheels are built for it. The GIL is released around parsing in nine of
  the ten packages (`json_fast` not yet) (#55).
- **email:** attachment file names could escape the output directory;
  `max_total_attachment_bytes` was accepted by the Python binding and
  ignored (#25, #55).
- **docx, pptx, email:** the same file could give different JSON, NDJSON and
  Arrow bytes on each parse (#57).
- **Flight servers:** an authenticated caller could tell whether a file
  outside the served root exists; `DoPut` and `PollFlightInfo` answered
  without a bearer token; error text could name the server's root (#61).
- **xlsx-write:** sheet names could be invalid or duplicated (#62).
- **pdf-write:** a small malformed source could produce gigabytes of output
  (#62).
- **geo:** a nested WMS capabilities document was collapsed into one layer
  (#60).

### Changed output

- **pdf:** page count and text change for incrementally saved files.
- **web:** table rows change shape for nested tables, empty cells and
  `rowspan`/`colspan`; a table reports only its own rows.
- **xsd (Arrow):** inherited elements and attributes, restriction
  attributes and typed text for `simpleContent` restrictions add columns;
  some leaf columns become structs; many columns become nullable; in a few
  schemas a path moves one level deeper. On 451 real schemas total column
  paths grow by 0.2 percent and none is lost.
- **xsd (validation):** a required child inherited from a base type is now
  reported when missing; children of an optional nested sequence are no
  longer reported.
- **docx:** `HeadingPairs` and `TitlesOfParts` keep every item, one per
  line; the keys `lpstr`, `i4` and `lpwstr` are gone. Symbol fonts inherited
  from a style map to the right characters. Attribute values that are not
  well-formed are read leniently instead of rejecting the document.
- **docx, pptx, email:** map keys are written in sorted order.
- **geo:** `SarMetadata.sensor` holds the platform name (for example
  `SENTINEL-1A`), not the instrument name. WMS layers carry inherited
  reference systems and bounding boxes.
- **python `geo_fast`:** new functions only; `geotiff_bands` returns one
  `bytes` object of little-endian f32 per band.
- **Flight:** a path that cannot be served answers `NotFound "file not
  found"` on every server, including paths outside the root (was
  `PermissionDenied`), directories and the empty path. `DoPut` and
  `PollFlightInfo` without a bearer answer `Unauthenticated`.
- **xlsx-write:** sheet names are cut to 31 UTF-16 units, cleaned and made
  unique; `Sheet::name` returns the name written. NaN and infinite numbers
  are written as `#NUM!`.

### New refusals

Inputs that were accepted, or that crashed or hung, and now return an error:

- **pdf:** a page that runs more than 16 MiB of nested Type 3 glyph
  procedure in one content stream (checked readers only; unchecked readers
  return what they read). Pages the render guard cannot bound are refused
  by the interpreted renderer (#43).
- **json:** validation stops with a single error when a work budget,
  nesting limit (64) or pattern budget is spent. A caller-raised nesting
  limit is capped at 128.
- **web:** documents nested deeper than 512 elements.
- **ontology:** more than 32 nested triple terms, more than 10,000 open
  brackets, or expanded term text over `max(256 MiB, 64 x source bytes)`.
- **xsd:** nothing that loaded before is refused; some schemas that were
  refused now load.
- **xlsx-write:** a cell outside the sheet grid, text over 32,767
  characters, a formula over 8,192, a JSON spec nested deeper than 32
  levels.
- **pdf-write:** encrypted sources, page-tree cycles, an object left
  unclosed, more than 16 MiB of repeated page dictionaries.
- **geo:** a WMS capabilities response over 128 MiB.
- **Node:** `geotiffBands` and the page-extraction functions reject numbers
  a cast would silently bend (negative, fractional, non-finite).

### Public Rust API

- **xsd-fast-core:** `ComplexType` gains the public fields `derivation`,
  `prohibited_attributes` and `empty_in_source`; struct literals of it
  break. Older serialized models still load.
- **ooxml-archive:** `part_names` yields names in byte order.
- **flight-ipc-config:** `resolve_path` refuses anything but a regular file.
- New constants for each limit above, and new functions in geo, xsd, email,
  pdf-write and xlsx-write. No existing function signature changed.

### Known gaps

- **json:** `$anchor`, `$id` base URIs, `$dynamicRef` and remote references
  are not resolved; such a `$ref` produces a warning and is skipped, so the
  schema accepts the value.
- **pdf:** three documented cases where text extraction and the renderer
  read different copies of an object in a malformed file. A glyph procedure
  is re-read for every code shown, so very long procedures shown many times
  are slow.
- **geo:** reference-system inheritance stops, without a signal, after
  64 MiB of copies; WFS responses and `get_map` have no size limit.
- **pdf-write:** sources with compressed cross-reference or object streams
  are not supported.
- **CI:** the geo crate's tests with the `http` feature and one PDF test
  file behind the `glyph-catalog` feature do not run in CI.
