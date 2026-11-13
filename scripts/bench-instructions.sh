#!/usr/bin/env bash
# Prints instructions retired, user CPU time and peak memory for one parse
# plus JSON of each benchmark input (xlsx, docx, pptx, email).
#
# Wall-clock benchmarks cannot resolve a small change on a busy machine;
# instructions retired move by about 1% from run to run. This is a measuring
# tool, not a gate: it exits 0 unless a build or a parse fails, and then it
# prints the failing command's output.
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

# Runs a command quietly; if it fails, prints what it wrote and stops.
quiet() {
  if ! "$@" >"$work/last.log" 2>&1; then
    echo "bench-instructions: command failed: $*" >&2
    cat "$work/last.log" >&2
    exit 1
  fi
}

for crate in xlsx docx pptx email; do
  package="${crate}-fast-core"
  input="$work/input.$crate"
  # The bench builds its input in memory; this makes it write it out. One
  # second of profiling is the cheapest way to run its setup.
  PARSERS_BENCH_DUMP_INPUT="$input" quiet cargo bench --locked -q -p "$package" --bench parse -- \
    --profile-time 1 json
  if [ ! -s "$input" ]; then
    echo "bench-instructions: the $package bench did not write its input to $input" >&2
    exit 1
  fi
  # Where the driver lands depends on CARGO_TARGET_DIR and cargo's
  # configuration, so the path is the one cargo reports for this build, never
  # a guess that could find a stale binary.
  quiet cargo build --locked --release -p "$package" --example "${crate}_bench_driver" \
    --message-format=json
  driver="$(sed -n 's/.*"name":"[a-z]*_bench_driver".*"executable":"\([^"]*\)".*/\1/p' "$work/last.log" | tail -n 1)"
  if [ -z "$driver" ] || [ ! -x "$driver" ]; then
    echo "bench-instructions: cargo reported no ${crate}_bench_driver executable for $package" >&2
    cat "$work/last.log" >&2
    exit 1
  fi
  for run in $(seq "$runs"); do
    printf '%s run=%s ' "$crate" "$run"
    measure "$driver" "$input"
  done
done
