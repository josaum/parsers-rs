# Changelog

Changes a consumer of the wheels, Node packages, crates or Flight servers
can observe. Each release also has generated notes listing every pull
request.

## Unreleased

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

### Known gaps

- **docx, xlsx, pptx, xsd, geo:** a reader remembers where it replaced
  bytes that are not UTF-8 in up to 4,096 separate runs per part. Past
  that, the runs merge into one, and a strictly read value inside the
  merged run that holds a real U+FFFD is treated as not UTF-8.

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
