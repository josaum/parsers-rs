# Sovereign parser verification policy

This Rust crate owns parser-specific checks. It has no dependency on Mycelia,
Atlas, their directory layouts, or LEIO's implementation. It implements LEIO's
version-1 native doctor JSON protocol and can also run independently:

```sh
cargo test --locked --manifest-path .leio-code/rust-doctors/Cargo.toml
cargo run --locked --manifest-path .leio-code/rust-doctors/Cargo.toml -- --check-root .
```

The catalog is `../native-doctors.json`. To run through a local LEIO host, build
the binary and explicitly trust it using `leio-code --repo "$PWD"
trust-doctor-pack --binary .leio-code/rust-doctors/target/debug/parsers-doctors`.

Policy follows this repository's published contract: seven Python bindings,
four Node bindings, shared Arrow dependency pins, existing documentation lint
ratchets, and CI on default/Arrow feature sets. It does not require imaginary
Node products for every Python package. These are source checks; CI still must
run the actual compilation, format, lint, parser and release-tooling tests.

`MIGRATION.json` records the original doctor source hashes and intentional
changes from the obsolete office-parsers layout. Consumer-specific OCR, layout,
GLiNER and integration policy is outside this repository.
