#!/usr/bin/env bash
# Build the parsers-rs Lean 4 formal proofs and refuse any `sorry` or non-standard axiom.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.elan/bin:$PATH"

lake build

if grep -rnE ':= *sorry\b|\bby +sorry\b|^[[:space:]]*sorry[[:space:]]*$' Parsers/*.lean Parsers.lean Audit.lean; then
  echo "sorry found in proofs" >&2
  exit 1
fi

axioms="$(lake env lean Audit.lean)"
echo "$axioms"
if grep -qE "sorryAx|ofReduceBool" <<<"$axioms"; then
  echo "a theorem depends on sorryAx or ofReduceBool" >&2
  exit 1
fi

echo "parsers-formal: all theorems machine-checked, 0 sorry."
