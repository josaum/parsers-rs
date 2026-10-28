/-
  Parsers/OoxmlInflate.lean

  Formal model and machine-checked proofs for `ooxml-archive/src/lib.rs`:
  `max_inflate` is an aggregate decompression budget across an entire OOXML
  archive view, enforced before exposing inflated bytes. A package with many
  individually-small parts cannot multiply the cap by splitting data across
  entries.
-/

namespace Parsers.OoxmlInflate

/-- Sum of uncompressed byte sizes across a list of ZIP archive entries. -/
def partSum : List Nat → Nat
  | [] => 0
  | p :: ps => p + partSum ps

@[simp] theorem partSum_nil : partSum [] = 0 := rfl
@[simp] theorem partSum_cons (p : Nat) (ps : List Nat) :
    partSum (p :: ps) = p + partSum ps := rfl

theorem partSum_append (xs ys : List Nat) :
    partSum (xs ++ ys) = partSum xs + partSum ys := by
  induction xs with
  | nil => simp
  | cons x xs ih =>
    simp [ih, Nat.add_assoc]

/-- Bounded inflate of a single archive part given `used` bytes already inflated
    in the current archive view and aggregate limit `maxInflate`.
    Mirrors `ooxml-archive`'s `take(remaining + 1)` check before exposing bytes. -/
def stepInflate (used part maxInflate : Nat) : Option Nat :=
  if used + part ≤ maxInflate then
    some (used + part)
  else
    none

/-- Sequential aggregate inflate over a list of archive parts starting from
    `used` bytes already inflated. -/
def aggregateInflate : List Nat → Nat → Nat → Option Nat
  | [], used, maxInflate =>
    if used ≤ maxInflate then some used else none
  | p :: ps, used, maxInflate =>
    if used + p ≤ maxInflate then
      aggregateInflate ps (used + p) maxInflate
    else
      none

/-- A single part inflate succeeds iff the new running total stays within `maxInflate`. -/
theorem stepInflate_eq_some_iff (used part maxInflate total : Nat) :
    stepInflate used part maxInflate = some total ↔
      used + part ≤ maxInflate ∧ total = used + part := by
  unfold stepInflate
  split
  · rename_i h
    constructor
    · intro hs
      injection hs with htot
      exact ⟨h, htot.symm⟩
    · intro ⟨_, htot⟩
      rw [htot]
  · rename_i h
    constructor
    · intro hs
      contradiction
    · intro ⟨hle, _⟩
      exact False.elim (h hle)

/-- A single part inflate returns `none` iff it would push the running total
    strictly past `maxInflate`. -/
theorem stepInflate_eq_none_iff (used part maxInflate : Nat) :
    stepInflate used part maxInflate = none ↔ maxInflate < used + part := by
  unfold stepInflate
  split
  · rename_i h
    constructor
    · intro hs; contradiction
    · intro hlt; omega
  · rename_i h
    constructor
    · intro _; omega
    · intro _; rfl

/-- Core soundness theorem for `ooxml-archive`:
    `aggregateInflate parts used maxInflate = some total` iff
    `used + partSum parts ≤ maxInflate` and `total = used + partSum parts`.
    Consequently, no sequence of parts can ever cause total inflated output to
    exceed `maxInflate`. -/
theorem aggregateInflate_eq_some_iff (parts : List Nat) (used maxInflate total : Nat) :
    aggregateInflate parts used maxInflate = some total ↔
      used + partSum parts ≤ maxInflate ∧ total = used + partSum parts := by
  induction parts generalizing used with
  | nil =>
    simp only [aggregateInflate, partSum_nil, Nat.add_zero]
    split
    · rename_i h
      constructor
      · intro hs
        injection hs with htot
        exact ⟨h, htot.symm⟩
      · intro ⟨_, htot⟩
        rw [htot]
    · rename_i h
      constructor
      · intro hs; contradiction
      · intro ⟨hle, _⟩; exact False.elim (h hle)
  | cons p ps ih =>
    simp only [aggregateInflate, partSum_cons]
    split
    · rename_i hstep
      rw [ih (used + p)]
      constructor
      · intro ⟨hle, htot⟩
        refine ⟨?_, ?_⟩ <;> omega
      · intro ⟨hle, htot⟩
        refine ⟨?_, ?_⟩ <;> omega
    · rename_i hstep
      constructor
      · intro hs; contradiction
      · intro ⟨hle, _⟩
        omega

/-- Direct corollary starting at `used = 0`: an archive view succeeds iff the sum
    of all inflated parts is at most `maxInflate`. -/
theorem archiveInflate_sound (parts : List Nat) (maxInflate total : Nat) :
    aggregateInflate parts 0 maxInflate = some total ↔
      partSum parts ≤ maxInflate ∧ total = partSum parts := by
  have h := aggregateInflate_eq_some_iff parts 0 maxInflate total
  simp only [Nat.zero_add] at h
  exact h

/-- Multi-part zip-bomb rejection theorem: whenever the cumulative uncompressed
    size across parts exceeds `maxInflate`, `aggregateInflate` is guaranteed to
    abort with `none` even if every individual part is smaller than `maxInflate`. -/
theorem archiveInflate_rejects_overflow (parts : List Nat) (maxInflate : Nat)
    (hover : maxInflate < partSum parts) :
    aggregateInflate parts 0 maxInflate = none := by
  cases hres : aggregateInflate parts 0 maxInflate with
  | none => rfl
  | some total =>
    have ⟨hle, _⟩ := (archiveInflate_sound parts maxInflate total).mp hres
    omega

/-- Once an archive prefix exceeds `maxInflate`, appending more parts remains rejected. -/
theorem aggregateInflate_prefix_failure_persists (prefixParts suffixParts : List Nat)
    (used maxInflate : Nat)
    (hfail : aggregateInflate prefixParts used maxInflate = none) :
    aggregateInflate (prefixParts ++ suffixParts) used maxInflate = none := by
  cases h : aggregateInflate (prefixParts ++ suffixParts) used maxInflate with
  | none => rfl
  | some total =>
    have ⟨hle, _⟩ :=
      (aggregateInflate_eq_some_iff (prefixParts ++ suffixParts) used maxInflate total).mp h
    rw [partSum_append] at hle
    have hpre : used + partSum prefixParts ≤ maxInflate := by omega
    have hsome :
        aggregateInflate prefixParts used maxInflate = some (used + partSum prefixParts) :=
      (aggregateInflate_eq_some_iff prefixParts used maxInflate (used + partSum prefixParts)).mpr
        ⟨hpre, rfl⟩
    rw [hfail] at hsome
    contradiction

/-- Worked example pinned by `decide`:
    Three parts `[400, 400, 300]` each fit inside a naive per-part limit of 500,
    but their aggregate size is 1100 > 1000, so `aggregateInflate` rejects them,
    whereas `[400, 400, 200]` succeeds with exact total 1000. -/
theorem worked_multipart_bomb_example :
    aggregateInflate [400, 400, 300] 0 1000 = none ∧
    aggregateInflate [400, 400, 200] 0 1000 = some 1000 := by
  decide

end Parsers.OoxmlInflate
