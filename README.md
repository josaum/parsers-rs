# parsers-rs

Sovereign, versioned, compiled parsers shared by multiple consumers, and
available to any other consumer.

This repository exists because the same parser crate names were being built
independently in two product repositories from sources that had drifted apart.
`json-fast-core` was 366 lines here and 124 lines there, both producing a wheel
called `json_fast-0.1.0`, with no pointer from the artifact back to the source
that produced it. A consumer could not tell which bytes it had.

The rule this repository enforces instead: **one source, one version, one
artifact, with provenance.**

## What belongs here

A crate belongs here when it is a **generic, read-only parse, a generic writer, or a generic
interop contract** — the same purpose in every consumer.

| In `parsers-rs` | Because |
| --- | --- |
| `json-fast-core`, `json-fast-py`, `json-fast-node`, `json-fast-flight` | SIMD JSON/JSONL and JSON-family decode, bindings, and Arrow Flight service |
| `xsd-fast-core`, `xsd-fast-py`, `xsd-fast-node`, `xsd-fast-flight` | XSD schema reading, Arrow derivation, bindings, and Arrow Flight service |
| `docx-fast-core`, `docx-fast-py`, `docx-fast-node`, `docx-fast-flight` | Read-only DOCX structure extraction, NDJSON streaming, bindings, and Arrow Flight service |
| `email-fast-core`, `email-fast-py`, `email-fast-node`, `email-fast-flight` | Read-only EML/MSG/mbox extraction with a bounded attachment budget, bindings, and Arrow Flight service |
| `xlsx-fast-core`, `xlsx-fast-py`, `xlsx-fast-node`, `xlsx-fast-flight` | Read-only XLSX workbook extraction, NDJSON and Arrow IPC streaming, bindings, and Arrow Flight service |
| `xlsx-fast-write`, `xlsx-fast-write-py`, `xlsx-fast-write-node` | Streaming XLSX writer with zero-copy XML sheet writing and shared-strings deduplication |
| `pptx-fast-core`, `pptx-fast-py`, `pptx-fast-node`, `pptx-fast-flight` | Read-only PPTX extraction, NDJSON, Arrow dataset, embedded media, bindings, and Arrow Flight service |
| `ooxml-archive` | Shared OOXML ZIP, bounded inflate, and relationship resolution |
| `pdf-fast-core`, `pdf-fast-py`, `pdf-fast-node` | Read-only PDF extraction: text, structure, tables, raster, Arrow, and bindings |
| `pdf-fast-write`, `pdf-fast-write-py`, `pdf-fast-write-node` | Direct PDF 1.7 writer for primitives, TrueType subsetting, images, and tables |
| `geo-fast-core`, `geo-fast-py`, `geo-fast-node` | Zero-copy GeoParquet, Shapefile, FlatGeobuf, and GeoJSON reader/writer to GeoArrow IPC |
| `ontology-fast-core`, `ontology-fast-py` | Canonical Turtle reading and indexed triple view with strict resource budgets |
| `web-fast-core` | Deterministic, side-effect-free HTML parser with Apache Arrow IPC/batches |
| `flight-ipc-config` | Shared Arrow Flight IPC compression and JWT authentication configuration |

`ooxml-archive` is the load-bearing one. `max_inflate` is an **aggregate**
budget for one archive view, not a per-part cap: a package with many
individually-small parts cannot multiply the limit by having many of them. The
restricted decoder that bounds output before exposing bytes is the `flate2` one,
so there is no fast path that inflates first and checks afterwards — a faster
decoder can only be reinstated by keeping that ordering.

The current source declares a release surface of **84 artifacts**: eleven Python
wheels × macOS arm64, Linux x86_64, Linux aarch64 and Windows x64, plus ten
Node tarballs × macOS arm64, Linux x86_64, Linux aarch64 and Windows x64.
Release publication requires the complete matrix after the reusable CI gate
passes. Both artifact families are bound to the commit by
`release-manifest.json` and the generator's `SHA256SUMS`; each artifact records
its own target rather than inheriting the manifest runner's host.

Observed on 2026-09-29, published `v2026.09.25.1` predates this expanded matrix:
it contains 21 wheels and 12 Node tarballs, without `ontology-fast` wheels or
`json-fast-node` tarballs. Its manifest and checksum file cover only wheels.
The source changes here do not retroactively certify or replace that release.

Node bindings build with `scripts/build-node.sh [crate ...]` (cargo, no
`@napi-rs/cli`) and are smoked with `node scripts/smoke-node.mjs`, which
generates each fixture in-process and asserts that a known string
round-trips through the parse.

## What must not belong here

A crate stays in its consumer when it carries that consumer's semantics or
types. Moving it here would invert the dependency graph and make a "shared"
repository into a component of one product.

| Stays in | Because |
| --- | --- |
| `owl-fast-core` (one consumer's reasoner projection) | It depends on that consumer's reasoner crate and emits its `RulePackage` `owl_axiom` rows. A shared crate cannot depend on one consumer. |
| `owl-fast-core` (another consumer's ontology chunker) | Entity extraction and subject chunking for RAG grounding. Product component, not a parser. |
| Consumer product crates (NER, OCR, layout, geometry, chunking, web extraction) | Product components — not parsers. |

`pdf-fast-py` carries one consumer-shaped feature: `reading-order` enables a
reading-order policy that needs the consumer's layout engine. It declares no
dependency here at all — a sovereign repository cannot name a consumer's crate —
so a consumer enables it and supplies the crate from its own workspace. Asking
for reading order on a build without it is an error rather than a silent fall
back to stream order.

The same rule is why `owl-fast-core` is absent: its reasoner form depends on a
consumer's reasoner crate, and a shared crate cannot depend on one consumer.
Applied consistently, that rule is what keeps this repository a shared plane
rather than a component of any one product.

`gliner` and `embed` are **optional features** that some consumers enable on
their document crates, and both must be removed when a core moves here.
They are not only feature flags: each gates a whole module (`adapter.rs`,
`embed.rs`) that depends on a consumer-side crate supplied from the consumer's
own workspace. Both have been removed from all cores here (docx, email, xlsx,
pptx, and pdf).

### The `owl-fast-core` name collision — measured, and not ours to merge

Two crates in two consumer repositories share this name and are **different
products**:

* one is an entity extractor and RAG chunker; its module doc states it is not
  a reasoner;
* the other emits Arrow IPC rule-package rows and depends on the consumer's
  reasoner crate.

The plan recorded here was **one crate with two features**:

```toml
owl-fast-core = { version = "x.y.z", features = ["reasoner-rows"] }
```

Measurement retired that plan. The two crates share **no public API at all** —
one exposes three functions, the other exposes sixteen, and not one name
appears on both sides. A single shared function exists in the whole
pair. A two-feature crate needs a common core for the features to wrap, and
there is none: this would be a merger of two products, not a reconciliation.

Both stay where they are, for the same reason in two directions: reasoner
projection is consumer semantics, and a RAG chunker is a product component.

**What is real is the packaging collision.** Two consumers publish a
distribution under the same name, importing as the same module, with different
contracts. On the flat module namespace the last wheel installed wins
silently, and a boundary that calls the other consumer's entry point breaks
every `.ttl`, `.nt`, and `.nq` upload while the module still imports.
Consumers guard against that at runtime by asserting each parser's entry
points rather than its importability.

The names are the defect — neither describes what its crate does. Renaming is a
consumer decision for the owning repositories, not something this one can
settle.

### The neutral reader here is `ontology-fast-core`

The crate that *does* describe reading ontologies is `ontology-fast-core`: it
depends only on `oxrdf` and `oxrdfio`, carries no consumer dependency, has no
name collision, and is already the declared migration target for a consumer's
ontology-catalog reader. It is the right candidate for this repository in this
area — not `owl-fast-core`. The sovereign release observed on 2026-09-29 did
not yet contain this binding's wheels. A consumer's local `ontology_fast`
import does not by itself prove adoption of a published sovereign release:
its wheel hash, release manifest and source pin must agree.

## Build

```bash
cargo test --workspace          # all crates
cargo build --release
```

Wheels:

```bash
cd json-fast-py && maturin build --release --out ../dist
cd xsd-fast-py  && maturin build --release --out ../dist
```

## Release

```bash
# embed each crate's CycloneDX SBOM in its wheels (authoritative: injection
# owns what a wheel carries), then stamp the manifest
cargo cyclonedx --manifest-path json-fast-core/Cargo.toml -f json --describe crate
python3 scripts/inject_sbom.py --dist dist
python3 scripts/release_manifest.py --out dist --require-complete
```

Cross-platform wheels use the toolchain the native reader already relies on:

```bash
rustup target add x86_64-unknown-linux-gnu
(cd json-fast-py && maturin build --release \
   --target x86_64-unknown-linux-gnu --zig --out ../dist)
```

This writes `dist/release-manifest.json` and `dist/SHA256SUMS`, which bind every wheel and Node tarball to its
SHA-256, reported version, the **source commit**, the build toolchain, and the
target triple. It refuses to write a manifest when:

* the working tree has uncommitted changes (pass `--allow-dirty` to record that
  explicitly instead),
* a wheel's reported version disagrees with its crate manifest — the exact
  drift that produced two different `json_fast-0.1.0` artifacts,
* a wheel is attributed to a consumer rather than to this repository.

ARM64 release wheels are built on `ubuntu-24.04-arm` inside an immutable
digest-pinned PyPA `manylinux2014_aarch64` container, using the repository's
exact Rust 1.98.1 toolchain and native GCC with the Cortex-A53 mitigation flag
preserved. The artifact gate installs and imports all eight wheels, verifies
their native target headers, and executes the existing Python binding contract
tests on the native Ubuntu host with PyArrow 25.0.1. This workflow contract has
local regression coverage; its actual cloud build has not been run by the
local verification below.

`--require-complete` additionally rejects missing or duplicate crate/target
pairs and missing or incorrectly attributed core SBOMs. It is mandatory in the
publication workflow. Omit it only for a partial local build, which is not proof
of the cross-platform release surface. `--allow-dirty` records local source
changes explicitly and must not be treated as a published release.
Every target stamp also checks the native payload: Linux requires a matching
64-bit little-endian ELF shared object, and macOS requires an ARM64 Mach-O
library. A filename or metadata tag cannot substitute for that architecture.

`cargo-cyclonedx` is required for a release. Each wheel carries **exactly one**
CycloneDX SBOM, for the core crate that defines its dependencies;
`scripts/inject_sbom.py` is authoritative about that and strips any SBOM that
arrived by another route, because a stray `cargo cyclonedx` in a binding
directory is enough for the wheel builder to package a second one. A missing or
duplicate SBOM fails the release rather than being recorded as an absence.
Injection replaces the SBOM set and regenerates wheel `RECORD` hashes and sizes
so the resulting wheel remains internally verifiable.

### Local packaging verification, 2026-09-29–2026-09-30

Local verification passed workspace tests and strict clippy both with default
features and the complete CI Arrow feature set, workspace formatting, and the
repository doctor. Release-generator regressions exercise a complete fixture
matrix and reject missing targets, duplicate targets, incorrect SBOMs, filename
drift and unusable source commits. The fixture matrix is not a cross-platform
binary build.

New macOS ARM64 `ontology-fast` and `json-fast-node` artifacts were built with
locked dependencies under `dist/local-build-2026-09-29/`. The ontology wheel
passed its Python contract suite in an isolated environment; the packed Node
tarball passed a native parse smoke after extraction. Linux x86_64 and ARM64
ontology wheels and JSON Node tarballs were also cross-built and
checked for archive integrity, native ELF architecture and artifact metadata.
The generated local manifest and checksums cover those artifacts and explicitly
record dirty source state.

Linux artifacts have not been executed: this Mac had no reachable local Docker
daemon during verification. ARM64 Zig builds have an additional safety limit:
upstream cargo-zigbuild filters Rust's `--fix-cortex-a53-843419` linker argument.
Their package checks do not establish that CPU-erratum mitigation. A release
must use a compatible native ARM64 linker and pass native execution tests;
running a cross-built binary on newer ARM hardware alone does not prove the
mitigation. See [the upstream filter change](https://github.com/rust-cross/cargo-zigbuild/pull/452)
and [Rust's mitigation requirement](https://github.com/rust-lang/rust/pull/155453).

These artifacts are development evidence, not publishable consumer pins. The
complete cross-platform release still requires the release workflow to run on
a clean committed revision; no release was published by this verification.

```bash
cargo install cargo-cyclonedx
```

## Consuming

A consumer ships **one release** of this repository, in two forms that must
name the same commit:

1. the wheels plus `release-manifest.json`, vendored into the consumer and
   installed by version and hash;
2. the source at `source.commit`, pinned as a git submodule, for any consumer
   Rust crate that links a core directly.

Do not vendor a private fork, and do not copy a crate into a consumer tree — a
copy is how the drift started. A consumer that needs something a core does not
do sends the change here and bumps the release.

```bash
# wheels, by hash from the manifest
pip install xlsx_fast==0.2.1 \
  --hash=sha256:b72174d0d9f5fa944998abc8b38822f0e957808916b16d462b636c23936e83c8

# source, pinned to the same commit the manifest names
git submodule add <remote-or-relative-url> vendor/parsers-rs/src
git -C vendor/parsers-rs/src checkout <source.commit>
```

```toml
# a consumer crate links the pinned source by path; the workspace here pins
# the transitive versions (arrow = 58.3.0 and friends)
json-fast-core = { path = "../../vendor/parsers-rs/src/json-fast-core", features = ["arrow"] }
```

The invariant is checked, not trusted. `scripts/verify_consumer_pin.py` fails a
consumer whose vendored wheels are not the manifest's bytes, whose vendored set
differs from the manifest's, whose manifest records a dirty tree, or whose
source pin is at any commit other than `source.commit`:

```bash
python3 vendor/parsers-rs/src/scripts/verify_consumer_pin.py --vendor vendor/parsers-rs
```

Because a release **is** a commit, a tooling-only change here does not move a
consumer's pin: the consumer stays on the release it ships until the next one.
A release must bump the version of every crate whose bytes changed since the
last tag: same wheel name, different bytes is the defect this repository
exists to end. The manifest cannot see earlier releases, so that rule is held
by the release procedure below and caught downstream by
`verify_consumer_pin.py`, which fails a wheel whose SHA-256 is not its
manifest's.

Release tags are `vYYYY.MM.DD` (`v2026.09.18`); crate versions are per crate
and live in the manifest, because the crates do not move in lockstep.

### CI and release

`.github/workflows/ci.yml` formats, lints (`-D warnings`) and tests the
workspace and the release scripts on every push. `.github/workflows/release.yml`
builds every binding for Apple Silicon, Linux x86_64 and Linux aarch64 on a
`v*` tag, embeds one SBOM per wheel, stamps the manifest, and publishes the set
with `SHA256SUMS` as release assets. Linux aarch64 is not optional: the API
image that consumes the wheels is multi-arch and cannot take a release that
lacks it.

`repository` in `Cargo.toml` is the remote every artifact inherits;
it must not point at a consumer.

## Provenance and cutover state

`json-fast` and `xsd-fast` were extracted from a consumer's
`office-parsers-rs/`; `docx-fast`, `email-fast`, and `xlsx-fast` were
reconciled from the two repositories that had been building them independently.
Every artifact in `dist/release-manifest.json` names its source commit.

A wheel in `dist/` is not the same claim as a wheel a consumer installs. The
distinction is recorded here because it is the difference between "released"
and "in use": a wheel in `dist/` is what this repository built and signed; a
wheel a consumer installs is whatever bytes that consumer actually runs, which
`verify_consumer_pin.py` checks against the manifest's SHA-256.

Consumers removed their in-tree copies of the seven cores, their bindings and
`ooxml-archive`; where a consumer links a core directly, it does so from the
vendored source pinned to the commit this manifest names. The remaining consumer-side gaps (a retired glyph persistence API some
consumer extractors still call, and consumer-specific XML shredding inside
downstream consumers) are product work in the consumers, not forks here.

Consumers install a release through a vendored `parsers-rs/` directory, with
the version and a SHA-256 recorded in their pinned requirements. Because a
find-links source records no hash in `uv.lock`, a hash-verifying install is a
separate documented command rather than something `uv sync` does for you — see
the vendored `parsers-rs/README.md` in the consumer.

Public surface is verified against consumer call sites before a cutover, not
after:

| API | Verified against |
| --- | --- |
| `json_fast.loads`, `loads_many` | JSON/JSONL loading and streaming paths across consumer pipelines |
| `xsd_fast.list_elements_bytes`, `describe_element_bytes` | Schema-driven discovery and upload flows |
| `docx_fast`, `email_fast` | Document upload and extraction workflows |
| `xlsx_fast`, `pptx_fast` | Compared wheel-to-wheel against the artifact each replaced, byte-identical on every case exercised; their NDJSON and Arrow surfaces cross-checked against the document projection on a fixture where no surface is vacuously empty |
