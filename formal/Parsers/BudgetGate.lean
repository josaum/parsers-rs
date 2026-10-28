/-
  Parsers/BudgetGate.lean

  Formal model and machine-checked proofs for multi-dimensional pre-materialization
  resource budgets in `json-fast-core/src/lib.rs`, `email-fast-core/src/lib.rs`,
  and `ontology-fast-core/src/lib.rs`:
  1. Input byte, nesting depth, token count, and retained output limits are
     checked before materializing output structures.
  2. Conservative retained-memory estimation before parse guarantees post-parse
     retained bytes never exceed `maxRetained`.
  3. Streaming item accumulation (JSONL records, email attachments, RDF triples)
     preserves both item-count and cumulative byte caps at every step.
-/

namespace Parsers.BudgetGate

/-- Resource limits configured on a parser instance. -/
structure Budget where
  maxBytes : Nat
  maxDepth : Nat
  maxTokens : Nat
  maxRetained : Nat
  deriving DecidableEq, Repr

/-- Observed or conservatively estimated resource usage of an input document. -/
structure Usage where
  bytes : Nat
  depth : Nat
  tokens : Nat
  retained : Nat
  deriving DecidableEq, Repr

/-- Propositional specification of a within-budget parse. -/
def WithinBudget (b : Budget) (u : Usage) : Prop :=
  u.bytes ≤ b.maxBytes ∧
  u.depth ≤ b.maxDepth ∧
  u.tokens ≤ b.maxTokens ∧
  u.retained ≤ b.maxRetained

/-- Executable pre-materialization budget gate. -/
def checkBudget (b : Budget) (u : Usage) : Bool :=
   decide (u.bytes ≤ b.maxBytes) &&
   decide (u.depth ≤ b.maxDepth) &&
   decide (u.tokens ≤ b.maxTokens) &&
   decide (u.retained ≤ b.maxRetained)

/-- `checkBudget` is both sound and complete for `WithinBudget`. -/
theorem checkBudget_iff_withinBudget (b : Budget) (u : Usage) :
    checkBudget b u = true ↔ WithinBudget b u := by
  unfold checkBudget WithinBudget
  simp [Bool.and_eq_true, decide_eq_true_eq, and_assoc]

/-- Monotonicity of `WithinBudget`: tightening usage or relaxing the budget
    preserves admissibility. -/
theorem withinBudget_mono (b₁ b₂ : Budget) (u₁ u₂ : Usage)
    (hw : WithinBudget b₁ u₁)
    (hbytes : u₂.bytes ≤ u₁.bytes)
    (hdepth : u₂.depth ≤ u₁.depth)
    (htokens : u₂.tokens ≤ u₁.tokens)
    (hret : u₂.retained ≤ u₁.retained)
    (hmaxB : b₁.maxBytes ≤ b₂.maxBytes)
    (hmaxD : b₁.maxDepth ≤ b₂.maxDepth)
    (hmaxT : b₁.maxTokens ≤ b₂.maxTokens)
    (hmaxR : b₁.maxRetained ≤ b₂.maxRetained) :
    WithinBudget b₂ u₂ := by
  rcases hw with ⟨hb, hd, ht, hr⟩
  refine ⟨?_, ?_, ?_, ?_⟩ <;> omega

/-- Conservative retained-output pre-check (`json-fast-core`
    `rejects_conservative_retained_output_budget_before_parse`):
    if the pre-parse conservative estimate `estRetained` upper-bounds actual
    retained memory (`actualRetained ≤ estRetained`) and passes `estRetained ≤ b.maxRetained`,
    then actual post-parse retained memory is guaranteed within `b.maxRetained`. -/
theorem conservativeRetained_sound (b : Budget) (uEst uActual : Usage)
    (hGate : checkBudget b uEst = true)
    (hBytesEq : uActual.bytes ≤ uEst.bytes)
    (hDepthLe : uActual.depth ≤ uEst.depth)
    (hTokensLe : uActual.tokens ≤ uEst.tokens)
    (hRetainedBound : uActual.retained ≤ uEst.retained) :
    WithinBudget b uActual := by
  have hw := (checkBudget_iff_withinBudget b uEst).mp hGate
  exact withinBudget_mono b b uEst uActual hw hBytesEq hDepthLe hTokensLe hRetainedBound
    (Nat.le_refl _) (Nat.le_refl _) (Nat.le_refl _) (Nat.le_refl _)

/-- Streaming accumulator state for bounded collections (attachments, JSONL rows,
    RDF triples): tracks `(count, totalBytes)`. -/
structure StreamState where
  count : Nat
  totalBytes : Nat
  deriving DecidableEq, Repr

/-- Streaming caps `(maxItems, maxTotalBytes)`. -/
structure StreamCap where
  maxItems : Nat
  maxTotalBytes : Nat
  deriving DecidableEq, Repr

def StreamValid (cap : StreamCap) (st : StreamState) : Prop :=
  st.count ≤ cap.maxItems ∧ st.totalBytes ≤ cap.maxTotalBytes

/-- Admit one more streamed item of size `itemBytes` iff both the item count and
    cumulative byte budget remain within `cap`. -/
def stepStream (cap : StreamCap) (st : StreamState) (itemBytes : Nat) : Option StreamState :=
  if st.count + 1 ≤ cap.maxItems ∧ st.totalBytes + itemBytes ≤ cap.maxTotalBytes then
    some { count := st.count + 1, totalBytes := st.totalBytes + itemBytes }
  else
    none

/-- `stepStream` preserves `StreamValid` on every accepted item. -/
theorem stepStream_preserves_valid (cap : StreamCap) (st next : StreamState) (itemBytes : Nat)
    (hstep : stepStream cap st itemBytes = some next) :
    StreamValid cap next := by
  unfold stepStream at hstep
  split at hstep
  · rename_i h
    injection hstep with hnext
    subst hnext
    exact h
  · contradiction

/-- Fold `stepStream` across a list of streamed item sizes. -/
def runStream (cap : StreamCap) : List Nat → StreamState → Option StreamState
  | [], st =>
    if st.count ≤ cap.maxItems ∧ st.totalBytes ≤ cap.maxTotalBytes then
      some st
    else
      none
  | sz :: rest, st =>
    match stepStream cap st sz with
    | some st' => runStream cap rest st'
    | none => none

/-- Any successful `runStream` execution yields a state satisfying `StreamValid`. -/
theorem runStream_sound (cap : StreamCap) (items : List Nat) (st final : StreamState)
    (hrun : runStream cap items st = some final) :
    StreamValid cap final := by
  induction items generalizing st with
  | nil =>
    unfold runStream at hrun
    split at hrun
    · rename_i h
      injection hrun with hfin
      subst hfin
      exact h
    · contradiction
  | cons sz rest ih =>
    unfold runStream at hrun
    cases hstep : stepStream cap st sz with
    | none =>
      rw [hstep] at hrun
      contradiction
    | some st' =>
      rw [hstep] at hrun
      exact ih st' hrun

/-- Worked example pinned by `decide`:
    A budget of `(maxBytes := 4096, maxDepth := 32, maxTokens := 512, maxRetained := 2048)`
    accepts usage `(1024, 12, 180, 1536)` and rejects `(1024, 33, 180, 1536)`. -/
theorem worked_budget_gate_example :
    let b : Budget := ⟨4096, 32, 512, 2048⟩
    checkBudget b ⟨1024, 12, 180, 1536⟩ = true ∧
    checkBudget b ⟨1024, 33, 180, 1536⟩ = false := by
  decide

end Parsers.BudgetGate
