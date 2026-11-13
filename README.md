# parsers-rs

Sovereign, versioned, high-performance document, data, geospatial, and ontology parsers, writers, and Arrow Flight services in Rust, with native Python (`abi3`) and Node.js (`N-API`) bindings across macOS, Linux, and Windows.

**Core rule:** *One source, one version, one artifact, with cryptographic provenance.*

Every published wheel and Node tarball is bound by `release-manifest.json` and `SHA256SUMS` to its exact source commit, SHA-256 digest, build toolchain, native binary header (`Mach-O ARM64`, `ELF64 x86_64/AArch64`, or `PE32+ AMD64`), and embedded CycloneDX SBOM.

---

## Workspace Catalog (42 Crates)

A crate belongs in `parsers-rs` when it provides a deterministic, side-effect-free parser, a generic format writer, or a shared Arrow IPC / Flight transport contract with zero dependencies on any downstream consumer.

| Domain | Rust Core / Writer | Python Binding (`abi3`) | Node-API Binding (`napi-2`) | Arrow Flight Service | Capabilities |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **JSON** | `json-fast-core` (`0.1.10`) | `json-fast-py` (`json_fast`) | `json-fast-node` | `json-fast-flight` (`0.1.4`) | SIMD JSON/JSONL parsing, JSON Schema validation, JSON-RPC 2.0, JSON:API, JWT/JWK split, JSON-LD context peek, Arrow batches |
| **XSD / XML** | `xsd-fast-core` (`0.2.8`) | `xsd-fast-py` (`xsd_fast`) | `xsd-fast-node` | `xsd-fast-flight` (`0.2.4`) | XSD schema parsing, element/type introspection, XML validation, and Arrow schema derivation |
| **DOCX** | `docx-fast-core` (`0.2.9`) | `docx-fast-py` (`docx_fast`) | `docx-fast-node` | `docx-fast-flight` (`0.2.5`) | Read-only DOCX structure extraction, tables, paragraphs, metadata, NDJSON & Arrow IPC streaming |
| **XLSX (Read)** | `xlsx-fast-core` (`0.2.9`) | `xlsx-fast-py` (`xlsx_fast`) | `xlsx-fast-node` | `xlsx-fast-flight` (`0.2.5`) | Read-only XLSX workbook extraction, typed cells, formulas, merged ranges, NDJSON & Arrow IPC streaming |
| **XLSX (Write)** | `xlsx-fast-write` (`0.2.3`) | `xlsx-fast-write-py` (`xlsx_fast_write`) | `xlsx-fast-write-node` | — | Streaming XLSX writer with zero-copy XML sheet writing, shared-strings deduplication, wrap/zebra/freeze-rows styling |
| **PPTX** | `pptx-fast-core` (`0.2.9`) | `pptx-fast-py` (`pptx_fast`) | `pptx-fast-node` | `pptx-fast-flight` (`0.2.5`) | Read-only PPTX slide, shape, table, notes, and embedded media extraction, NDJSON & Arrow datasets |
| **PDF (Read)** | `pdf-fast-core` (`0.2.8`) | `pdf-fast-py` (`pdf_fast`) | `pdf-fast-node` | — | Read-only PDF text, glyph catalog, vector/raster images, CCITT/LZW, tables, page rendering, and Arrow batches |
| **PDF (Write)** | `pdf-fast-write` (`0.2.3`) | `pdf-fast-write-py` (`pdf_fast_write`) | `pdf-fast-write-node` | — | Direct PDF 1.7 writer for vector primitives, TrueType font subsetting, raster images, and structured tables |
| **Email** | `email-fast-core` (`0.2.10`) | `email-fast-py` (`email_fast`) | `email-fast-node` | `email-fast-flight` (`0.2.4`) | Read-only EML (RFC 5322), OLE/CFB `.msg`, and `mbox` extraction with bounded attachment budgets and Arrow IPC |
| **Geospatial** | `geo-fast-core` (`0.2.4`) | `geo-fast-py` (`geo_fast`) | `geo-fast-node` | — | GeoParquet, Shapefile, FlatGeobuf, GeoJSON, GeoTIFF, SAR, thermal, hyperspectral, WMS/WFS, and GeoArrow IPC |
| **Ontology** | `ontology-fast-core` (`0.1.4`) | `ontology-fast-py` (`ontology_fast`) | — | — | Canonical Turtle/RDF reading and indexed triple store with strict memory and triple-count budgets |
| **Web / HTML** | `web-fast-core` (`0.1.4`) | — | — | — | Deterministic, side-effect-free HTML5 DOM & table extraction with Apache Arrow record batches |
| **OOXML ZIP** | `ooxml-archive` (`0.1.7`) | — | — | — | Shared OOXML ZIP reader, aggregate `max_inflate` decompression bomb guard, and relationship resolution |
| **Flight Config** | `flight-ipc-config` (`0.1.3`) | — | — | — | Shared Arrow Flight IPC compression (`LZ4`/`ZSTD`), JWT bearer authentication, and transport configuration |

---

## Security & Resource-Budget Guarantees

1. **Aggregate Decompression Budgets (`ooxml-archive`)**:
   `max_inflate` is enforced as an **aggregate budget across the entire archive view**, not merely per ZIP entry. Archive parts are inflated through a bounded `flate2` reader that caps output bytes *before* exposing buffers, preventing multi-part zip-bomb amplification.
2. **Strict Input & Structural Budgets**:
   `json-fast-core`, `email-fast-core`, `ontology-fast-core`, and `web-fast-core` enforce explicit byte, nesting-depth, token-count, attachment, and triple-count limits prior to full materialization.
3. **Hardened Arrow Flight Services (`*-flight`)**:
   All six Arrow Flight services (`docx`, `xlsx`, `pptx`, `email`, `xsd`, `json`) are strictly read-only (`do_put` and mutating actions fail loud), require valid JWT bearer authorization before request parsing, reject symlinks during directory enumeration across both POSIX and Windows, prevent path traversal outside `--root`, and normalize Flight tickets to canonical `/` separators on every OS.
4. **Pinned Arrow Wire Format**:
   All workspace members share an exact Apache Arrow wire pin (`arrow = "=58.4.1"`, `arrow-array = "=58.4.1"`, `arrow-ipc = "=58.4.1"`, `arrow-schema = "=58.4.1"`, `arrow-flight = "=58.4.1"`), enforced by `.leio-code/rust-doctors`.

---

## Limits and guarantees

The limits each parser enforces on its input, with the default and the name
that holds it in the source. A limit that is a `ParseOptions` / `JsonLimits`
field can be changed by the caller; a `const` cannot.

| Crate | Limit | Default | Source |
|-------|-------|---------|--------|
| `ooxml-archive` | Bytes inflated across one archive view | 128 MiB | `DEFAULT_MAX_INFLATE`, `ooxml-archive/src/lib.rs` |
| `docx-fast-core` | Inflate budget | 128 MiB | `ParseOptions::max_inflate`, `docx-fast-core/src/lib.rs` |
| | Nested table depth | 32 | `MAX_TABLE_DEPTH` |
| | `basedOn` style links followed | 64 | `MAX_STYLE_CHAIN` |
| `xlsx-fast-core` | Inflate budget | 128 MiB | `ParseOptions::max_inflate`, `xlsx-fast-core/src/lib.rs` |
| `pptx-fast-core` | Inflate budget | 128 MiB | `ParseOptions::max_inflate`, `pptx-fast-core/src/lib.rs` |
| `email-fast-core` | One attachment / all attachments | 100 MiB / 100 MiB | `ParseOptions::max_attachment_size`, `max_total_attachment_bytes`, `email-fast-core/src/lib.rs` |
| | MIME nesting depth | 32 | `MAX_MIME_DEPTH` |
| | Header-decoding work per message | 3,000,000,000 characters walked | `MAX_HEADER_DECODE_COST` |
| | Multipart boundary length | 200 bytes | `MAX_BOUNDARY_LEN` |
| `json-fast-core` | Input size | 8 MiB | `DEFAULT_MAX_INPUT_BYTES`, `json-fast-core/src/lib.rs` |
| | Nesting depth | 64 | `DEFAULT_MAX_NESTING_DEPTH` |
| | Tokens | 1,000,000 | `DEFAULT_MAX_TOKENS` |
| | JSONL lines / records | 100,000 / 100,000 | `DEFAULT_MAX_LINES`, `DEFAULT_MAX_RECORDS` |
| | Retained bytes / Arrow IPC bytes | 64 MiB / 64 MiB | `DEFAULT_MAX_RETAINED_BYTES`, `DEFAULT_MAX_IPC_BYTES` |
| | JSON Schema `$ref` hops | 32 | `MAX_REF_DEPTH`, `json-fast-core/src/schema.rs` |
| `xsd-fast-core` | Schema bytes, root plus includes | 32 MiB | `DEFAULT_MAX_SCHEMA_BYTES`, `xsd-fast-core/src/parser.rs` |
| | One `schemaLocation` value | 4 KiB | `DEFAULT_MAX_SCHEMA_LOCATION_BYTES` |
| | Included or imported files | 64 | `ParseOptions::max_includes` |
| | Include chain depth | 32 | `MAX_INCLUDE_NESTING` |
| | Nesting of type-bearing constructs | 64 | `MAX_NESTING_DEPTH` |
| | Validation findings reported | 100 | `ValidateOptions::max_errors`, `xsd-fast-core/src/validator.rs` |
| `web-fast-core` | Input size | 32 MiB | `ParseOptions::max_input_bytes`, `web-fast-core/src/lib.rs` |
| | Emitted nodes | 250,000 | `ParseOptions::max_nodes` |
| | Emitted text | 32 MiB | `ParseOptions::max_text_bytes` |
| `geo-fast-core` | GeoJSON nesting depth | 32 | `MAX_NESTING_DEPTH`, `geo-fast-core/src/geojson.rs` |
| | GeoTIFF decoded bytes per input byte | 8 uncompressed, 32,768 compressed | `UNCOMPRESSED_EXPANSION`, `COMPRESSED_EXPANSION`, `geo-fast-core/src/geotiff.rs` |
| | GeoTIFF band data returned | the larger of 4 bytes per input byte and twice the `tiff` decoder's buffer cap | `BAND_BYTES_PER_INPUT_BYTE`, `BAND_BYTES_PER_DECODED_BYTE`, `max_band_bytes` |

`pdf-fast-core` has limits of its own; they are documented next to the
constants in that crate and are not repeated here.

Two names that look like limits and are not: `ParseOptions::max_inflate` in
`email-fast-core` is accepted and has no effect, because that crate inflates
nothing; and `MAX_BATCH_TEXT_BYTES` in `xlsx-fast-core/src/arrow.rs` (64 MiB)
is the size at which an Arrow batch is closed, which a batch passes by up to
one cell, not a bound on the input.

**What is tested.** Every core crate has `tests/malformed_input_smoke.rs`: a
deterministic, fixed-seed byte mutator over valid seed documents
(`tools/mutation_smoke.rs`) that fails on any panic, runs under an allocator
that aborts past 1 GiB of live memory (`ALLOCATION_CAP_BYTES`), and feeds
deeply nested inputs to the parser on a 256 KiB thread stack
(`SMALL_STACK_BYTES`). The workspace tests run on Linux, macOS and Windows on
every pull request (`rust` and `rust-platforms` in `.github/workflows/ci.yml`).
Coverage-guided libFuzzer targets live in the `fuzz/` directory of each of
those crates except `pdf-fast-core` and `ontology-fast-core`, and start from
the same seed documents. They cover the in-memory readers built with default
features. They do not cover the Arrow writers (the `arrow` feature), the
memory-mapped and path-based readers, or `geo-fast-core`'s HTTP clients; the
smoke tests above are what exercises the Arrow writers. The targets run weekly
in `.github/workflows/fuzz.yml`, and for a few seconds on a pull request that
changes a fuzz crate.

The same workflow runs, weekly and not on pull requests, a build with the
minimum supported Rust version and Miri over the hand-written SIMD scans:
the SSE2 and NEON versions of `find_first_digit` (`xlsx-fast-core`) and of
`find_obj_markers_simd` (`pdf-fast-core`). The AVX2 version of
`find_first_digit` is not interpreted by Miri, and no other `unsafe` code is:
the remaining blocks are memory maps and the Arrow C data interface of the
Python bindings.

**What is not guaranteed.**

- The limits bound memory and recursion. They do not bound wall-clock time,
  except where a constant above says so (`MAX_HEADER_DECODE_COST`,
  `MAX_BOUNDARY_LEN`): a large input within its limits takes time in
  proportion to its size.
- On malformed input the goal is an error or a partial result, never a panic,
  stack overflow or unbounded allocation. What a parser extracts from a
  damaged file is not specified and may change between versions.
- Fuzzing and mutation testing find bugs; they do not prove there are none,
  and the readers listed above as not fuzzed have no fuzzing at all.
- No sandboxing and no constant-time behaviour. A caller that parses hostile
  files should still isolate the process and set its own time limit.
- The minimum supported Rust version is the `rust-version` in `Cargo.toml`
  (1.94); releases are built with the toolchain pinned in
  `rust-toolchain.toml`.

---

## Sovereign Boundary Rules

To keep `parsers-rs` a clean, shared foundation rather than a component coupled to any single product:

- **No Consumer Dependencies**: Crates in this workspace never depend on consumer-specific crates (such as product NER models, RAG chunkers, embedding pipelines, or domain reasoners).
- **Optional Feature Gates**:
  - Consumer-specific `gliner` and `embed` modules have been excluded from all core crates.
  - `pdf-fast-py` exposes an opt-in `reading-order` feature flag for consumers that supply their own layout engine at build time; requesting reading order on a build without that feature returns an explicit error rather than silently falling back to stream order.
- **Why `ontology-fast-core` Replaces `owl-fast-core` Here**:
  Two downstream consumers historically had incompatible crates named `owl-fast-core` (one an entity/RAG chunker, the other a reasoner rule-package projector) with zero shared public API. Both product-specific crates remain in their respective consumer repositories, while `ontology-fast-core` provides the neutral, dependency-clean RDF/Turtle reader here.

---

## Release Matrix & Provenance (84 Artifacts)

Every tagged release (`vYYYY.MM.DD`) builds and publishes **84 binary artifacts** after the reusable CI verification workflow passes:

| Artifact Family | Count | Targets |
| :--- | :--- | :--- |
| **Python Wheels (`cp38-abi3`)** | **44** (11 crates × 4 targets) | `aarch64-apple-darwin` (`macosx_11_0_arm64`)<br>`x86_64-unknown-linux-gnu` (`manylinux_2_17_x86_64`)<br>`aarch64-unknown-linux-gnu` (`manylinux_2_17_aarch64`)<br>`x86_64-pc-windows-msvc` (`win_amd64`) |
| **Node-API Tarballs (`napi-2`)** | **40** (10 crates × 4 targets) | `darwin-arm64` (`aarch64-apple-darwin`)<br>`linux-x64-gnu` (`x86_64-unknown-linux-gnu`)<br>`linux-arm64-gnu` (`aarch64-unknown-linux-gnu`)<br>`win32-x64-msvc` (`x86_64-pc-windows-msvc`) |

### Release Integrity Gates

- **Native Linux ARM64 Build & Cortex-A53 Mitigation**:
  `aarch64-unknown-linux-gnu` wheels are built natively on `ubuntu-24.04-arm` inside an immutable digest-pinned `manylinux2014_aarch64` container using Rust `1.98.1` and native GCC with `-Wl,--fix-cortex-a53-843419` (avoiding `cargo-zigbuild`'s upstream filter that strips that erratum flag), then smoke-tested natively with `pytest` and `pyarrow==25.0.1`.
- **Single Authoritative CycloneDX SBOM per Wheel**:
  `scripts/inject_sbom.py` generates and embeds exactly one CycloneDX SBOM (`<dist-info>/sboms/core.json`) per wheel corresponding to its Rust core/writer crate, strips any stray SBOMs, and recomputes `RECORD` SHA-256 digests and byte sizes.
- **Complete Matrix & Native Header Verification**:
  `scripts/release_manifest.py --out dist --require-complete` verifies that all 84 artifacts are present without duplicates, checks that every `.so`, `.pyd`, and `.node` binary matches its target's native `ELF64`, `Mach-O ARM64`, or `PE32+ AMD64` machine header, refuses dirty git trees, and writes `dist/release-manifest.json` and `dist/SHA256SUMS`.

---

## Development & Verification

### Rust Workspace & Architectural Doctors

```bash
# Format check
cargo fmt --all --check

# Lint all 42 crates and all targets (default and Arrow feature sets)
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked \
  --features docx-fast-core/arrow,xlsx-fast-core/arrow,pptx-fast-core/arrow,pdf-fast-core/arrow,email-fast-core/arrow,json-fast-core/arrow,xsd-fast-core/arrow,web-fast-core/arrow

# Run workspace tests
cargo test --workspace --locked
cargo test --workspace --locked \
  --features docx-fast-core/arrow,xlsx-fast-core/arrow,pptx-fast-core/arrow,pdf-fast-core/arrow,email-fast-core/arrow,json-fast-core/arrow,xsd-fast-core/arrow,web-fast-core/arrow

# Run sovereign repository doctors
cargo test --locked --manifest-path .leio-code/rust-doctors/Cargo.toml
cargo run --locked --manifest-path .leio-code/rust-doctors/Cargo.toml -- --check-root .

# Run machine-checked Lean 4 proofs & axiom audit (0 sorry)
./formal/check.sh
```

### Python Wheels, Node Bindings & Contract Tests

```bash
# Build a Python wheel locally
(cd json-fast-py && maturin build --release --locked --out ../dist)

# Build and smoke-test all 10 Node-API bindings
scripts/build-node.sh
node scripts/smoke-node.mjs

# Run release & workflow regression suite
uv run --with pytest pytest tests -q
```

### Cross-Platform & Windows Compatibility

The workspace is verified for POSIX and Windows (`x86_64-pc-windows-msvc` / `x86_64-pc-windows-gnu`) compatibility:
- `.gitattributes` enforces LF line endings (`eol=lf`) across source files, scripts, and parser fixtures so `core.autocrlf=true` on Windows never mutates test bytes or shell scripts.
- All Python release and verification tools (`scripts/release_manifest.py`, `scripts/inject_sbom.py`, `scripts/verify_consumer_pin.py`, `scripts/consumer_surface_check.py`) use explicit UTF-8 encoding, normalize backslash paths, and resolve Windows virtualenv layouts (`Scripts/python.exe`, `win_amd64`).

---

## Releasing & Consuming

### Release Versioning Policy

1. **Per-Crate Version Bump**: Before tagging a release, bump the version of every crate whose source changed since the previous tag (along with its matching `-py` and `-node` `Cargo.toml`, `pyproject.toml`, and `package.json` manifests). Same wheel filename with different bytes is forbidden.
2. **Calendar Release Tag**: Tag the clean commit on `main` as `vYYYY.MM.DD` (for example, `v2026.10.28`) and push the tag to trigger `.github/workflows/release.yml`.

### Consuming a Release

Downstream consumers pin a release in two forms that must reference the **exact same `source.commit`**:

1. **Vendored Artifacts**: Vendor the release wheels/tarballs together with `release-manifest.json` and install by exact version and `--hash=sha256:...`.
2. **Submodule Source Pin**: For consumer Rust crates that link a `*-core` or `*-write` crate directly, check out the repository submodule at `source.commit`.

Verify consumer pin integrity in CI with:

```bash
python3 vendor/parsers-rs/src/scripts/verify_consumer_pin.py --vendor vendor/parsers-rs
python3 vendor/parsers-rs/src/scripts/consumer_surface_check.py --dist dist --consumer .
```

## Third-party licences

`web-fast-core` links four crates under the Mozilla Public License 2.0, through
its HTML parser `scraper`: `cssparser`, `cssparser-macros`, `selectors` and
`dtoa-short`. They are used unmodified, at the versions `Cargo.lock` records,
and their source is published on crates.io (`https://crates.io/crates/<name>`)
and in the `servo/rust-cssparser`, `servo/stylo` and `upsuper/dtoa-short`
repositories on GitHub. MPL-2.0 asks a distributor of a binary that contains
them to say where that source is; it puts no condition on the rest of the
code. `deny.toml` allows MPL-2.0 for these four crates and for no other.

## License

Licensed under either of

- MIT license ([LICENSE](LICENSE))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option. Every wheel and Node tarball carries both texts: wheels under `*.dist-info/licenses/`, tarballs at the package root. The release workflow refuses to publish an artifact that is missing either one.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this repository shall be dual licensed as above, without any additional terms or conditions.
