/-
  formal/Audit.lean

  Environment-wide axiom audit for `parsers-rs`.
  Every declaration in a `Parsers` module may depend only on Lean's standard
  axioms (`propext`, `Quot.sound`, `Classical.choice`) — never `sorryAx`
  (`sorry`), never `Lean.ofReduceBool` (`native_decide`), and never a custom
  `axiom`.
-/
import Lean
import Parsers

open Lean Elab Command

def allowedAxioms : List Name := [``propext, ``Quot.sound, ``Classical.choice]

elab "#audit_axioms" : command => do
  let env ← getEnv
  let mut checked := 0
  let mut constructive := 0
  let mut offenders : Array String := #[]
  for (name, info) in env.constants.map₁ do
    let some idx := env.getModuleIdxFor? name | continue
    let modName := env.header.moduleNames[idx.toNat]!
    unless (`Parsers).isPrefixOf modName do continue
    checked := checked + 1
    if info matches .axiomInfo _ then
      offenders := offenders.push s!"{name} is an axiom (declared in {modName})"
      continue
    let axioms ← liftCoreM <| collectAxioms name
    if axioms.isEmpty then constructive := constructive + 1
    let bad := axioms.filter (fun a => !allowedAxioms.contains a)
    unless bad.isEmpty do
      offenders := offenders.push s!"{name} ({modName}) depends on {bad.toList}"
  if checked == 0 then
    throwError "axiom audit: no declaration from a Parsers module was found"
  unless offenders.isEmpty do
    let sorted := offenders.qsort (· < ·)
    throwError "axiom audit failed, {sorted.size} of {checked} declarations:\n{"\n".intercalate sorted.toList}"
  logInfo m!"axiom audit: {checked} declarations, none outside {allowedAxioms}; {constructive} depend on no axiom at all"

#audit_axioms

#print axioms Parsers.OoxmlInflate.stepInflate_eq_some_iff
#print axioms Parsers.OoxmlInflate.stepInflate_eq_none_iff
#print axioms Parsers.OoxmlInflate.aggregateInflate_eq_some_iff
#print axioms Parsers.OoxmlInflate.archiveInflate_sound
#print axioms Parsers.OoxmlInflate.archiveInflate_rejects_overflow
#print axioms Parsers.OoxmlInflate.aggregateInflate_prefix_failure_persists
#print axioms Parsers.OoxmlInflate.worked_multipart_bomb_example
#print axioms Parsers.BudgetGate.checkBudget_iff_withinBudget
#print axioms Parsers.BudgetGate.withinBudget_mono
#print axioms Parsers.BudgetGate.conservativeRetained_sound
#print axioms Parsers.BudgetGate.stepStream_preserves_valid
#print axioms Parsers.BudgetGate.runStream_sound
#print axioms Parsers.BudgetGate.worked_budget_gate_example
#print axioms Parsers.FlightSecurity.doPut_always_rejected
#print axioms Parsers.FlightSecurity.retrieval_rejects_unauthenticated
#print axioms Parsers.FlightSecurity.unauthenticated_doGet_ticket_independent
#print axioms Parsers.FlightSecurity.collectMatchingRegularFiles_only_regular
#print axioms Parsers.FlightSecurity.collectMatchingRegularFiles_skips_symlinks
#print axioms Parsers.FlightSecurity.normalizeSepChar_ne_backslash
#print axioms Parsers.FlightSecurity.normalizeSepChar_idem
#print axioms Parsers.FlightSecurity.normalizeTicketChars_no_backslash
#print axioms Parsers.FlightSecurity.normalizeTicketChars_idem
#print axioms Parsers.FlightSecurity.worked_flight_security_example
#print axioms Parsers.ReleaseProvenance.expectedReleaseArtifactCount_eq_84
#print axioms Parsers.ReleaseProvenance.countSboms_stripSboms
#print axioms Parsers.ReleaseProvenance.stripSboms_idem
#print axioms Parsers.ReleaseProvenance.countSboms_injectSbom
#print axioms Parsers.ReleaseProvenance.injectSbom_idem
#print axioms Parsers.ReleaseProvenance.dirty_tree_rejected
#print axioms Parsers.ReleaseProvenance.incomplete_matrix_rejected
#print axioms Parsers.ReleaseProvenance.canStampCompleteRelease_iff
#print axioms Parsers.ReleaseProvenance.verifyConsumerPin_iff
#print axioms Parsers.ReleaseProvenance.verified_consumers_agree
