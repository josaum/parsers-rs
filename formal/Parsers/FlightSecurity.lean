/-
  Parsers/FlightSecurity.lean

  Formal model and machine-checked proofs for the 6 Arrow Flight services
  (`docx-fast-flight`, `xlsx-fast-flight`, `pptx-fast-flight`, `email-fast-flight`,
  `xsd-fast-flight`, `json-fast-flight`) and `flight-ipc-config`:
  1. Bearer JWT authentication is enforced before request payload parsing.
  2. Every service is strictly read-only (`do_put` always fails loud).
  3. Directory listing skips symlinks (`collect_*_paths_skips_symlinks` on both
     POSIX and Windows).
  4. Relative Flight tickets normalize Windows `\` separators to canonical `/`
     separators idempotently and without leaking backslashes.
-/

namespace Parsers.FlightSecurity

/-- Authentication outcome from `flight-ipc-config`. -/
inductive AuthState where
  | missing
  | invalid
  | authenticated
  deriving DecidableEq, Repr

/-- Arrow Flight RPC operations exposed by the `*-flight` servers. -/
inductive FlightOp where
  | handshake
  | listFlights
  | getFlightInfo (ticketValid : Bool)
  | doGet (ticketValid : Bool)
  | doAction (isCapabilityAction : Bool) (payloadValid : Bool)
  | doPut (payloadValid : Bool)
  deriving DecidableEq, Repr

/-- Deterministic response classification of the Flight service contract. -/
inductive FlightResponse where
  | ok
  | unauthenticated
  | invalidArgument
  | unimplementedReadOnly
  deriving DecidableEq, Repr

/-- Reference dispatch semantics shared by all six `*-flight` services. -/
def dispatch (auth : AuthState) (op : FlightOp) : FlightResponse :=
  match op with
  | .doPut _ => .unimplementedReadOnly
  | .handshake => .ok
  | .listFlights =>
    match auth with
    | .authenticated => .ok
    | _ => .unauthenticated
  | .getFlightInfo ticketValid =>
    match auth with
    | .authenticated => if ticketValid then .ok else .invalidArgument
    | _ => .unauthenticated
  | .doGet ticketValid =>
    match auth with
    | .authenticated => if ticketValid then .ok else .invalidArgument
    | _ => .unauthenticated
  | .doAction isCap payloadValid =>
    match auth with
    | .authenticated =>
      if isCap || payloadValid then .ok else .invalidArgument
    | _ => .unauthenticated

/-- Read-only invariant (`do_put_fails_loud_as_read_only`):
    `doPut` is unconditionally rejected as `.unimplementedReadOnly` for every
    authentication state and payload. -/
theorem doPut_always_rejected (auth : AuthState) (payloadValid : Bool) :
    dispatch auth (.doPut payloadValid) = .unimplementedReadOnly :=
  rfl

/-- Auth-before-payload invariant (`retrieval_rejects_missing_authorization_before_request_parsing`):
    for any retrieval or action RPC (`listFlights`, `getFlightInfo`, `doGet`, `doAction`),
    if `auth ≠ .authenticated`, the server returns `.unauthenticated` without
    depending on `ticketValid` or `payloadValid`. -/
theorem retrieval_rejects_unauthenticated (auth : AuthState)
    (hauth : auth ≠ .authenticated) (ticketValid : Bool) :
    dispatch auth (.getFlightInfo ticketValid) = .unauthenticated ∧
    dispatch auth (.doGet ticketValid) = .unauthenticated ∧
    dispatch auth .listFlights = .unauthenticated := by
  cases auth with
  | missing => exact ⟨rfl, rfl, rfl⟩
  | invalid => exact ⟨rfl, rfl, rfl⟩
  | authenticated => contradiction

/-- Unauthenticated retrieval outcome is independent of ticket validity:
    malformed tickets cannot be probed by unauthenticated callers. -/
theorem unauthenticated_doGet_ticket_independent (auth : AuthState)
    (hauth : auth ≠ .authenticated) (t₁ t₂ : Bool) :
    dispatch auth (.doGet t₁) = dispatch auth (.doGet t₂) := by
  have ⟨_, h₁, _⟩ := retrieval_rejects_unauthenticated auth hauth t₁
  have ⟨_, h₂, _⟩ := retrieval_rejects_unauthenticated auth hauth t₂
  rw [h₁, h₂]

/-- Filesystem entry kind encountered during `--root` directory traversal. -/
inductive EntryKind where
  | regular
  | directory
  | symlink
  deriving DecidableEq, Repr

structure FsEntry where
  name : String
  kind : EntryKind
  extMatches : Bool
  deriving DecidableEq, Repr

/-- Directory listing filter used across all `*-flight` servers:
    keeps only regular files (`kind == .regular`) whose extension matches.
    Symlinks (`.symlink`) are unconditionally skipped on both POSIX and Windows. -/
def collectMatchingRegularFiles : List FsEntry → List FsEntry
  | [] => []
  | e :: es =>
    if e.kind == .regular && e.extMatches then
      e :: collectMatchingRegularFiles es
    else
      collectMatchingRegularFiles es

/-- Every file returned by `collectMatchingRegularFiles` is a `.regular` file
    (never a `.symlink` or `.directory`). -/
theorem collectMatchingRegularFiles_only_regular (entries : List FsEntry) (e : FsEntry)
    (hmem : e ∈ collectMatchingRegularFiles entries) :
    e.kind = .regular ∧ e.extMatches = true := by
  induction entries with
  | nil =>
    simp [collectMatchingRegularFiles] at hmem
  | cons hd tl ih =>
    unfold collectMatchingRegularFiles at hmem
    split at hmem
    · rename_i hcond
      simp only [Bool.and_eq_true, beq_iff_eq] at hcond
      cases List.mem_cons.mp hmem with
      | inl heq =>
        subst heq
        exact hcond
      | inr htl =>
        exact ih htl
    · exact ih hmem

/-- Symlink exclusion theorem (`collect_*_paths_skips_symlinks`):
    no entry with `e.kind = .symlink` ever appears in `collectMatchingRegularFiles entries`. -/
theorem collectMatchingRegularFiles_skips_symlinks (entries : List FsEntry) (e : FsEntry)
    (hmem : e ∈ collectMatchingRegularFiles entries) :
    e.kind ≠ .symlink := by
  have ⟨hreg, _⟩ := collectMatchingRegularFiles_only_regular entries e hmem
  rw [hreg]
  decide

/-- Cross-platform path separator normalization for Flight tickets:
    replaces Windows `\` with canonical `/`. -/
def normalizeSepChar (c : Char) : Char :=
  if c == '\\' then '/' else c

def normalizeTicketChars : List Char → List Char
  | [] => []
  | c :: cs => normalizeSepChar c :: normalizeTicketChars cs

/-- `normalizeSepChar` never outputs a backslash `'\\'`. -/
theorem normalizeSepChar_ne_backslash (c : Char) :
    normalizeSepChar c ≠ '\\' := by
  unfold normalizeSepChar
  split
  · decide
  · rename_i h
    intro heq
    subst heq
    exact h rfl

/-- `normalizeSepChar` is idempotent. -/
theorem normalizeSepChar_idem (c : Char) :
    normalizeSepChar (normalizeSepChar c) = normalizeSepChar c := by
  unfold normalizeSepChar
  split <;> rfl

/-- `normalizeTicketChars` never contains `'\\'` in its output. -/
theorem normalizeTicketChars_no_backslash (cs : List Char) :
    '\\' ∉ normalizeTicketChars cs := by
  induction cs with
  | nil =>
    simp [normalizeTicketChars]
  | cons c cs ih =>
    unfold normalizeTicketChars
    intro hmem
    cases List.mem_cons.mp hmem with
    | inl heq =>
      exact normalizeSepChar_ne_backslash c heq.symm
    | inr htl =>
      exact ih htl

/-- `normalizeTicketChars` is idempotent on all character sequences. -/
theorem normalizeTicketChars_idem (cs : List Char) :
    normalizeTicketChars (normalizeTicketChars cs) = normalizeTicketChars cs := by
  induction cs with
  | nil => rfl
  | cons c cs ih =>
    simp [normalizeTicketChars, normalizeSepChar_idem, ih]

/-- Worked example pinned by `decide`:
    1. Windows path `"nested\\sub\\report.xlsx"` normalizes to `"nested/sub/report.xlsx"`.
    2. Symlink entry `"leak.xlsx"` is excluded by `collectMatchingRegularFiles`. -/
theorem worked_flight_security_example :
    normalizeTicketChars "nested\\sub\\report.xlsx".toList = "nested/sub/report.xlsx".toList ∧
    collectMatchingRegularFiles
      [ ⟨"real.xlsx", .regular, true⟩
      , ⟨"leak.xlsx", .symlink, true⟩
      , ⟨"notes.txt", .regular, false⟩ ] =
      [ ⟨"real.xlsx", .regular, true⟩ ] := by
  decide

end Parsers.FlightSecurity
