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
| Document/email/pdf read cores (planned) | Same read-only output contract in both consumers |
| `*-node` bindings (planned) | Thin, per-platform, versioned alongside the core |

## What must not belong here

A crate stays in its consumer when it carries that consumer's semantics or
types. Moving it here would invert the dependency graph and make a "shared"
repository into a component of one product.

| Stays in | Because |
| --- | --- |
| **Atlas**: `owl-fast-core` (reasoner projection) | It depends on `atlas-reasoner` and emits `RulePackage` `owl_axiom` rows. A shared crate cannot depend on one consumer. |
| **Mycelia**: `gliner-core`, `gliner2-core`, OCR models, `geom-core`, `layout-fast-*`, `web-fast-core`, `chunk-fast-core`, `fca-fast-*` | Product components — NER, OCR, layout and chunking — not parsers. |

`gliner` is an **optional feature** on Mycelia's docx/pdf/email/xlsx/pptx
crates. If those cores move here, that feature must be removed from the core and
expressed by the consumer, or the core acquires a Mycelia dependency.

### The `owl-fast-core` name collision

Two crates in two repositories currently share this name and are **different
products**:

* Mycelia's is an entity extractor and RAG chunker (`Entity`, `EntityType`,
  `SubjectChunk`); its module doc states it is not a reasoner.
* Atlas's is a canonical reader that emits Arrow IPC `RulePackage` rows and
  depends on `atlas-reasoner`.

The agreed target is **one crate with two features**:

```toml
owl-fast-core = { version = "x.y.z", features = ["atlas-reasoner-rows"] }
```

The default build stays a neutral reader with no consumer dependency; the
`atlas-reasoner-rows` feature adds the Arrow projection and pulls
`atlas-reasoner`. That resolves the collision without moving Atlas's reasoner
types into this repository. It is **not implemented here yet** — this release
ships `json-fast` and `xsd-fast` only.

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

`cargo-cyclonedx` is not installed here; the manifest records that SBOMs are
absent rather than implying they exist. Install it before a release consumers
depend on:

```bash
cargo install cargo-cyclonedx
```

## Consuming

Pin the **version**, verify the **hash** from the manifest, and do not vendor a
private fork — a fork is how the drift started.

```bash
pip install json_fast==0.1.0 --hash=sha256:64914c6ca83030b3d55c7f83732c4ebe1b3bf0d72097143296f1f056075b4c19
```

For Rust consumers, depend on a tag and let the workspace pin transitive
versions:

```toml
json-fast-core = { git = "<remote>", tag = "v0.1.0" }
```

## Provenance of the first release

`json-fast` and `xsd-fast` were extracted from Atlas Brain's
`office-parsers-rs/` at the commit recorded in `dist/release-manifest.json`.
Their public Python surface was verified against the call sites in both
consumers before publishing:

| Wheel | API | Atlas call site | Verified |
| --- | --- | --- | --- |
| `json_fast` | `loads`, `loads_many` | `cartridges/atlas/_jsonl.py`, `margin.py`, `feeds/pacvue.py`, `dataplane.py` | ✅ |
| `xsd_fast` | `list_elements_bytes`, `describe_element_bytes` | `app/atlas_demo/uploads.py` | ✅ |

Consumer repositories still build these crates from their own local source.
Switching them to this release is a separate, deliberate step: it changes their
build inputs, so it should land with a pinned version and hash rather than as a
side effect of this repository appearing.
