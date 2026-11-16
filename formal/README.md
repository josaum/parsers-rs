# formal/ — Lean 4 Proofs for `parsers-rs`

Machine-checked Lean 4 proofs (`leanprover/lean4:v4.35.0`, pure Lean core library, zero external packages, zero `sorry`) for the load-bearing safety, resource-budget, Flight security, and release-provenance invariants of `parsers-rs`.

```bash
./formal/check.sh
```

## What these proofs cover

Each module is a Lean model of an algorithm, written by hand to mirror the Rust or Python code named beside it. The theorems are proved about that model. Nothing extracts the model from the source or checks the two against each other mechanically, so a proof here shows the design holds, not that the implementation follows it.

The link between model and code is kept by review, and the code itself is checked by its test suites: unit and integration tests, and the malformed-input tests under each crate's `tests/malformed_input_smoke.rs`. When a counterpart changes, its model has to be updated by hand.

## Verified Modules & Theorems

| Module | Theorems | Rust / Python Counterpart | Invariant |
| :--- | :--- | :--- | :--- |
| `Parsers.OoxmlInflate` | `stepInflate_eq_some_iff`, `stepInflate_eq_none_iff`, `aggregateInflate_eq_some_iff`, `archiveInflate_sound`, `archiveInflate_rejects_overflow`, `aggregateInflate_prefix_failure_persists`, `worked_multipart_bomb_example` | `ooxml-archive/src/lib.rs` | `max_inflate` is enforced across the aggregate sum of all inflated ZIP entries before exposing bytes; multi-part zip bombs are always rejected. |
| `Parsers.BudgetGate` | `checkBudget_iff_withinBudget`, `withinBudget_mono`, `conservativeRetained_sound`, `stepStream_preserves_valid`, `runStream_sound`, `worked_budget_gate_example` | `json-fast-core`, `email-fast-core`, `ontology-fast-core` | Multi-dimensional byte/depth/token/retained-memory gates and streaming accumulators never exceed configured caps. |
| `Parsers.FlightSecurity` | `doPut_always_rejected`, `retrieval_rejects_unauthenticated`, `unauthenticated_doGet_ticket_independent`, `collectMatchingRegularFiles_only_regular`, `collectMatchingRegularFiles_skips_symlinks`, `normalizeSepChar_ne_backslash`, `normalizeSepChar_idem`, `normalizeTicketChars_no_backslash`, `normalizeTicketChars_idem`, `worked_flight_security_example` | `flight-ipc-config`, `*-flight/src/main.rs` | All 6 Flight servers are read-only, enforce JWT auth before payload parsing, skip symlinks during directory enumeration, and normalize Windows `\` paths to `/` idempotently. |
| `Parsers.ReleaseProvenance` | `expectedReleaseArtifactCount_eq_84`, `countSboms_stripSboms`, `stripSboms_idem`, `countSboms_injectSbom`, `injectSbom_idem`, `dirty_tree_rejected`, `incomplete_matrix_rejected`, `canStampCompleteRelease_iff`, `verifyConsumerPin_iff`, `verified_consumers_agree` | `scripts/inject_sbom.py`, `scripts/release_manifest.py`, `scripts/verify_consumer_pin.py` | Complete 84-artifact matrix, authoritative single CycloneDX SBOM per wheel, dirty-tree rejection, and byte-identical consumer pin verification. |
