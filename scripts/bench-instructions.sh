#!/usr/bin/env bash
# Prints instructions retired, user CPU time and peak memory for one parse
# plus JSON of each benchmark input (xlsx, docx, pptx, email).
#
# Wall-clock benchmarks cannot resolve a small change on a busy machine;
# instructions retired move by about 1% from run to run. This is a measuring
# tool, not a gate: it always exits 0 unless a build or a parse fails.
#
# Usage: scripts/bench-instructions.sh [runs]   (default 3)
# Needs /usr/bin/time -l (macOS) or perf (Linux). See docs/PERFORMANCE.md.
set -euo pipefail

runs="${1:-3}"
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$root"

measure() { # <driver> <input>
  if [ "$(uname)" = "Darwin" ]; then
    /usr/bin/time -l "$1" "$2" 2>&1 >/dev/null | awk '
      /instructions retired/ {i=$1} /maximum resident set size/ {m=$1} / user / {u=$3}
      END {printf "instructions=%s user_s=%s peak_rss_bytes=%s\n", i, u, m}'
  else
    perf stat -x, -e instructions:u "$1" "$2" 2>&1 >/dev/null | awk -F, '
      /instructions/ {printf "instructions=%s\n", $1}'
  fi
}

for crate in xlsx docx pptx email; do
  package="${crate}-fast-core"
  input="$work/input.$crate"
  # The bench builds its input in memory; this makes it write it out. One
  # second of profiling is the cheapest way to run its setup.
  PARSERS_BENCH_DUMP_INPUT="$input" cargo bench --locked -q -p "$package" --bench parse -- \
    --profile-time 1 json >/dev/null 2>&1
  cargo build --locked -q --release -p "$package" --example bench_driver
  # Every crate's driver has the same name: keep a copy before the next build.
  cp "target/release/examples/bench_driver" "$work/driver_$crate"
  for run in $(seq "$runs"); do
    printf '%s run=%s ' "$crate" "$run"
    measure "$work/driver_$crate" "$input"
  done
done
