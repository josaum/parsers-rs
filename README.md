# parsers-rs

Sovereign, versioned, compiled parsers shared by **Atlas Brain** and
**Mycelia**, and available to any other consumer.

This repository exists because the same parser crate names were being built
independently in two product repositories from sources that had drifted apart.
`json-fast-core` was 366 lines here and 124 lines there, both producing a wheel
called `json_fast-0.1.0`, with no pointer from the artifact back to the source
that produced it. A consumer could not tell which bytes it had.

The rule this repository enforces instead: **one source, one version, one
artifact, with provenance.**

## What belongs here

A crate belongs here when it is a **generic, read-only parse or a generic
interop contract** — the same purpose in every consumer.

| In `parsers-rs` | Because |
| --- | --- |
| `json-fast-core`, `json-fast-py` | SIMD JSON/JSONL and JSON-family decode; same in both consumers |
| `xsd-fast-core`, `xsd-fast-py` | XSD schema reading and Arrow derivation; no consumer semantics |
| `docx-fast-core`, `docx-fast-py` | Read-only DOCX structure extraction and NDJSON streaming |
| `email-fast-core`, `email-fast-py` | Read-only EML/MSG/mbox extraction, with a bounded attachment budget |
| `xlsx-fast-core`, `xlsx-fast-py` | Read-only XLSX workbook extraction, NDJSON and Arrow IPC streaming |
| `pptx-fast-core`, `pptx-fast-py` | Read-only PPTX extraction, NDJSON, Arrow dataset, embedded media |
| `ooxml-archive` | Shared OOXML ZIP, bounded inflate, and relationship resolution |
| `pdf-fast-core`, `pdf-fast-py` | Read-only PDF extraction: text, structure, tables, raster, Arrow |
| `*-node` bindings (planned) | Thin, per-platform, versioned alongside the core |

`ooxml-archive` is the load-bearing one. `max_inflate` is an **aggregate**
budget for one archive view, not a per-part cap: a package with many
individually-small parts cannot multiply the limit by having many of them. The
restricted decoder that bounds output before exposing bytes is the `flate2` one,
so there is no fast path that inflates first and checks afterwards — a faster
decoder can only be reinstated by keeping that ordering.

The shipped surface is **14 artifacts**: seven crates × macOS arm64 and Linux
x86_64.

## What must not belong here

A crate stays in its consumer when it carries that consumer's semantics or
types. Moving it here would invert the dependency graph and make a "shared"
repository into a component of one product.

| Stays in | Because |
| --- | --- |
| **Atlas**: `owl-fast-core` (reasoner projection) | It depends on `atlas-reasoner` and emits `RulePackage` `owl_axiom` rows. A shared crate cannot depend on one consumer. |
| **Mycelia**: `owl-fast-core` (ontology chunker) | Entity extraction and subject chunking for RAG grounding. Product component, not a parser. |
| **Mycelia**: `gliner-core`, `gliner2-core`, OCR models, `geom-core`, `layout-fast-*`, `web-fast-core`, `chunk-fast-core`, `fca-fast-*` | Product components — NER, OCR, layout and chunking — not parsers. |

`pdf-fast-py` carries one consumer-shaped feature: `reading-order` enables a
reading-order policy that needs the consumer's layout engine. It declares no
dependency here at all — a sovereign repository cannot name a consumer's crate —
so a consumer enables it and supplies the crate from its own workspace. Asking
for reading order on a build without it is an error rather than a silent fall
back to stream order.

The same rule is why `owl-fast-core` is absent: its Atlas form depends on
`atlas-reasoner`, and a shared crate cannot depend on one consumer. Applied
consistently, that rule is what keeps this repository a shared plane rather than
a component of either product.

`gliner` and `embed` are **optional features** on Mycelia's docx/pdf/email/
xlsx/pptx crates, and both must be removed when a core moves here. They are not
only feature flags: each gates a whole module (`adapter.rs`, `embed.rs`) that
depends on `gliner-core` or `mycelia-client`. Removed so far from docx, email,
xlsx, and pptx; `pdf` still carries them.

### The `owl-fast-core` name collision — measured, and not ours to merge

Two crates in two repositories share this name and are **different products**:

* Mycelia's is an entity extractor and RAG chunker (`Entity`, `EntityType`,
  `SubjectChunk`); its module doc states it is not a reasoner.
* Atlas's emits Arrow IPC `RulePackage` rows and depends on `atlas-reasoner`.

The plan recorded here was **one crate with two features**:

```toml
owl-fast-core = { version = "x.y.z", features = ["atlas-reasoner-rows"] }
```

Measurement retired that plan. The two crates share **no public API at all** —
Atlas exposes three functions (`from_name`, `parse_ontology_to_arrow_ipc`,
`parse_ontology_to_arrow_ipc_with_limits`), Mycelia exposes sixteen, and not one
name appears on both sides. A single shared function exists in the whole pair.
A two-feature crate needs a common core for the features to wrap, and there is
none: this would be a merger of two products, not a reconciliation.

Both stay where they are, for the same reason in two directions: reasoner
projection is consumer semantics, and a RAG chunker is a product component.

**What is real is the packaging collision.** Both repositories publish the
distribution `owl-fast`, importing as `owl_fast`, with different contracts —
Atlas's `parse_ontology_ipc` against Mycelia's `parse_file` /
`chunk_by_subject`. On the flat module namespace the last wheel installed wins
silently, and Atlas's upload boundary calls `owl_fast.parse_ontology_ipc`, so
the wrong package breaks every `.ttl`, `.nt`, and `.nq` upload while the module
still imports. Atlas now guards against that at runtime by asserting each
parser's entry points rather than its importability.

The names are the defect — neither describes what its crate does. Renaming is a
consumer decision for the two repositories, not something this one can settle.

### The neutral reader here is `ontology-fast-core`

The crate that *does* describe reading ontologies is `ontology-fast-core`: it
depends only on `oxrdf` and `oxrdfio`, carries no consumer dependency, has no
name collision, and is already the declared migration target for Atlas's
`catalog._subject_blocks`. It is the right candidate for this repository in this
area — not `owl-fast-core`. Its Python binding is not yet wired into Atlas's app
path.

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
python3 scripts/release_manifest.py --out dist
```

Cross-platform wheels use the toolchain the native reader already relies on:

```bash
rustup target add x86_64-unknown-linux-gnu
(cd json-fast-py && maturin build --release \
   --target x86_64-unknown-linux-gnu --zig --out ../dist)
```

This writes `dist/release-manifest.json`, which binds every wheel to its
SHA-256, reported version, the **source commit**, the build toolchain, and the
target triple. It refuses to write a manifest when:

* the working tree has uncommitted changes (pass `--allow-dirty` to record that
  explicitly instead),
* a wheel's reported version disagrees with its crate manifest — the exact
  drift that produced two different `json_fast-0.1.0` artifacts,
* a wheel is attributed to a consumer rather than to this repository.

`cargo-cyclonedx` is required for a release. Each wheel carries **exactly one**
CycloneDX SBOM, for the core crate that defines its dependencies;
`scripts/inject_sbom.py` is authoritative about that and strips any SBOM that
arrived by another route, because a stray `cargo cyclonedx` in a binding
directory is enough for the wheel builder to package a second one. A missing or
duplicate SBOM fails the release rather than being recorded as an absence.

```bash
cargo install cargo-cyclonedx
```

## Consuming

Pin the **version**, verify the **hash** from the manifest, and do not vendor a
private fork — a fork is how the drift started.

```bash
pip install xlsx_fast==0.2.1 \
  --hash=sha256:b72174d0d9f5fa944998abc8b38822f0e957808916b16d462b636c23936e83c8
```

For Rust consumers, depend on a tag and let the workspace pin transitive
versions:

```toml
json-fast-core = { git = "<remote>", tag = "v0.1.0" }
```

## Provenance and cutover state

`json-fast` and `xsd-fast` were extracted from Atlas Brain's
`office-parsers-rs/`; `docx-fast`, `email-fast`, and `xlsx-fast` were
reconciled from the two repositories that had been building them independently.
Every artifact in `dist/release-manifest.json` names its source commit.

A wheel in `dist/` is not the same claim as a wheel a consumer installs. The
distinction is recorded here because it is the difference between "released"
and "in use":

| Wheel | Atlas | Mycelia |
| --- | --- | --- |
| `json_fast` | installs this release | builds from its own source |
| `xsd_fast` | installs this release | builds from its own source |
| `docx_fast` | installs this release | builds from its own source |
| `email_fast` | installs this release | builds from its own source |
| `xlsx_fast` | installs this release | builds from its own source |
| `pptx_fast` | installs this release | builds from its own source |
| `pdf_fast` | installs this release | builds from its own source |

Atlas consumes these through `vendor/parsers-rs/` with a version and a SHA-256
in `app/requirements/parsers-rs-pinned.txt`. Because a find-links source records
no hash in `uv.lock`, a hash-verifying install is a separate documented command
rather than something `uv sync` does for you — see `vendor/parsers-rs/README.md`
in the consumer.

Public surface is verified against consumer call sites before a cutover, not
after:

| API | Verified against |
| --- | --- |
| `json_fast.loads`, `loads_many` | Atlas `cartridges/atlas/_jsonl.py`, `margin.py`, `feeds/pacvue.py`, `dataplane.py` |
| `xsd_fast.list_elements_bytes`, `describe_element_bytes` | Atlas `app/atlas_demo/uploads.py` |
| `docx_fast`, `email_fast` | Atlas upload suite |
| `xlsx_fast`, `pptx_fast` | Compared wheel-to-wheel against the artifact each replaced, byte-identical on every case exercised; their NDJSON and Arrow surfaces cross-checked against the document projection on a fixture where no surface is vacuously empty |
