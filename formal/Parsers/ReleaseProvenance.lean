/-
  Parsers/ReleaseProvenance.lean

  Formal model and machine-checked proofs for the sovereign release & provenance
  pipeline (`scripts/inject_sbom.py`, `scripts/release_manifest.py`,
  `scripts/verify_consumer_pin.py`):
  1. Complete release matrix size: 11 Python bindings × 4 targets + 10 Node
     bindings × 4 targets = 84 artifacts.
  2. `injectSbom` strips any pre-existing SBOM entries and injects the single
     authoritative core CycloneDX SBOM (`countSboms = 1`) idempotently.
  3. `stampManifest` refuses dirty git trees and incomplete matrices.
  4. `verifyConsumerPin` guarantees commit-pin agreement, clean provenance, and
     digest equality between vendored artifacts and the release manifest.
-/

namespace Parsers.ReleaseProvenance

/-- Number of Python binding crates in the sovereign release matrix. -/
def pythonBindingCount : Nat := 11

/-- Number of Node-API binding crates in the sovereign release matrix. -/
def nodeBindingCount : Nat := 10

/-- Number of target platforms (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
    `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`). -/
def targetPlatformCount : Nat := 4

/-- Total required artifacts in a complete sovereign release. -/
def expectedReleaseArtifactCount : Nat :=
  pythonBindingCount * targetPlatformCount + nodeBindingCount * targetPlatformCount

/-- The sovereign release surface has exactly 84 artifacts (44 wheels + 40 Node tarballs). -/
theorem expectedReleaseArtifactCount_eq_84 :
    expectedReleaseArtifactCount = 84 := by
  decide

/-- Simplified archive entry inside a Python wheel `.whl` zip archive. -/
structure WheelEntry where
  path : String
  isSbom : Bool
  sha256 : Nat
  deriving DecidableEq, Repr

/-- Count how many embedded SBOM files exist in a wheel archive. -/
def countSboms : List WheelEntry → Nat
  | [] => 0
  | e :: es => (if e.isSbom then 1 else 0) + countSboms es

/-- Strip all existing SBOM entries (`scripts/inject_sbom.py` authoritative cleanup). -/
def stripSboms : List WheelEntry → List WheelEntry
  | [] => []
  | e :: es =>
    if e.isSbom then stripSboms es else e :: stripSboms es

/-- `stripSboms` leaves zero SBOM entries in the wheel. -/
theorem countSboms_stripSboms (entries : List WheelEntry) :
    countSboms (stripSboms entries) = 0 := by
  induction entries with
  | nil => rfl
  | cons e es ih =>
    unfold stripSboms
    split
    · exact ih
    · rename_i hnot
      simp [countSboms, hnot, ih]

/-- `stripSboms` is idempotent. -/
theorem stripSboms_idem (entries : List WheelEntry) :
    stripSboms (stripSboms entries) = stripSboms entries := by
  induction entries with
  | nil => rfl
  | cons e es ih =>
    simp only [stripSboms]
    split
    · exact ih
    · rename_i hnot
      simp only [stripSboms, hnot, ih]
      rfl

/-- Authoritative SBOM injection (`scripts/inject_sbom.py`):
    strips any stray SBOMs and prepends the single core CycloneDX SBOM. -/
def injectSbom (entries : List WheelEntry) (coreSbomDigest : Nat) : List WheelEntry :=
  { path := "dist-info/sboms/core.json", isSbom := true, sha256 := coreSbomDigest } ::
    stripSboms entries

/-- Every wheel processed by `injectSbom` carries **exactly one** SBOM
    regardless of how many stray SBOMs were present beforehand. -/
theorem countSboms_injectSbom (entries : List WheelEntry) (coreSbomDigest : Nat) :
    countSboms (injectSbom entries coreSbomDigest) = 1 := by
  unfold injectSbom countSboms
  simp [countSboms_stripSboms]

/-- `injectSbom` is idempotent: running SBOM injection a second time with the
    same core SBOM produces the exact same entry list. -/
theorem injectSbom_idem (entries : List WheelEntry) (coreSbomDigest : Nat) :
    injectSbom (injectSbom entries coreSbomDigest) coreSbomDigest =
      injectSbom entries coreSbomDigest := by
  unfold injectSbom
  simp [stripSboms, stripSboms_idem]

/-- Release manifest candidate state evaluated by `scripts/release_manifest.py`. -/
structure ReleaseCandidate where
  commitKnown : Bool
  dirty : Bool
  artifactCount : Nat
  hasDuplicates : Bool
  allNativeHeadersValid : Bool
  allWheelSbomsValid : Bool
  deriving DecidableEq, Repr

/-- Executable release gate for `release_manifest.py --require-complete`. -/
def canStampCompleteRelease (rc : ReleaseCandidate) : Bool :=
  rc.commitKnown &&
  !rc.dirty &&
  decide (rc.artifactCount = expectedReleaseArtifactCount) &&
  !rc.hasDuplicates &&
  rc.allNativeHeadersValid &&
  rc.allWheelSbomsValid

/-- A dirty working tree can never stamp a complete release. -/
theorem dirty_tree_rejected (rc : ReleaseCandidate) (hdirty : rc.dirty = true) :
    canStampCompleteRelease rc = false := by
  unfold canStampCompleteRelease
  simp [hdirty]

/-- An incomplete artifact matrix (`artifactCount ≠ 84`) can never stamp a complete release. -/
theorem incomplete_matrix_rejected (rc : ReleaseCandidate)
    (hincomplete : rc.artifactCount ≠ expectedReleaseArtifactCount) :
    canStampCompleteRelease rc = false := by
  unfold canStampCompleteRelease
  simp [hincomplete]

/-- Exact soundness characterization of `canStampCompleteRelease`. -/
theorem canStampCompleteRelease_iff (rc : ReleaseCandidate) :
    canStampCompleteRelease rc = true ↔
      rc.commitKnown = true ∧
      rc.dirty = false ∧
      rc.artifactCount = 84 ∧
      rc.hasDuplicates = false ∧
      rc.allNativeHeadersValid = true ∧
      rc.allWheelSbomsValid = true := by
  unfold canStampCompleteRelease expectedReleaseArtifactCount
  unfold pythonBindingCount nodeBindingCount targetPlatformCount
  simp [Bool.and_eq_true, decide_eq_true_eq, and_assoc]

/-- Consumer pin state checked by `scripts/verify_consumer_pin.py`. -/
structure ConsumerPinState where
  manifestDirty : Bool
  manifestCommit : Nat
  submoduleCommit : Nat
  manifestDigests : List Nat
  vendoredDigests : List Nat
  deriving DecidableEq, Repr

/-- Executable verification predicate in `scripts/verify_consumer_pin.py`. -/
def verifyConsumerPin (st : ConsumerPinState) : Bool :=
  !st.manifestDirty &&
  decide (st.submoduleCommit = st.manifestCommit) &&
  decide (st.vendoredDigests = st.manifestDigests)

/-- `verifyConsumerPin` succeeds iff the manifest records a clean tree, the
    vendored source submodule matches `source.commit`, and the vendored wheel/tarball
    digests match the manifest digests exactly. -/
theorem verifyConsumerPin_iff (st : ConsumerPinState) :
    verifyConsumerPin st = true ↔
      st.manifestDirty = false ∧
      st.submoduleCommit = st.manifestCommit ∧
      st.vendoredDigests = st.manifestDigests := by
  unfold verifyConsumerPin
  simp [Bool.and_eq_true, decide_eq_true_eq, and_assoc]

/-- No silent byte drift across verified consumers sharing the same manifest:
    any two consumer trees that pass `verifyConsumerPin` against the same
    `manifestCommit` and `manifestDigests` run byte-for-byte identical artifacts
    and link the exact same source commit. -/
theorem verified_consumers_agree (c₁ c₂ : ConsumerPinState)
    (hv₁ : verifyConsumerPin c₁ = true)
    (hv₂ : verifyConsumerPin c₂ = true)
    (hcommit : c₁.manifestCommit = c₂.manifestCommit)
    (hdigests : c₁.manifestDigests = c₂.manifestDigests) :
    c₁.submoduleCommit = c₂.submoduleCommit ∧
    c₁.vendoredDigests = c₂.vendoredDigests := by
  have ⟨_, hs₁, hd₁⟩ := (verifyConsumerPin_iff c₁).mp hv₁
  have ⟨_, hs₂, hd₂⟩ := (verifyConsumerPin_iff c₂).mp hv₂
  constructor
  · rw [hs₁, hs₂, hcommit]
  · rw [hd₁, hd₂, hdigests]

end Parsers.ReleaseProvenance
