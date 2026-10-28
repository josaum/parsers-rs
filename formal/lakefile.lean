import Lake
open Lake DSL

-- Pure Lean 4 core library only: no Mathlib, no external packages.
package parsers where

@[default_target]
lean_lib Parsers where

-- Built by default after `Parsers`; walks the Lean environment and fails the
-- build if any declaration in a `Parsers` module uses `sorryAx`,
-- `Lean.ofReduceBool` (`native_decide`), or declares an `axiom`.
@[default_target]
lean_lib Audit where
