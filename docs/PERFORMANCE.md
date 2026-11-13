# Performance

How the speed of the parsers is measured, what the numbers were when the
benchmarks were added, and what a change made for speed has to leave alone.

## Running the benchmarks

Five crates carry a [criterion](https://crates.io/crates/criterion) bench:

```bash
cargo bench -p xlsx-fast-core
cargo bench -p docx-fast-core
cargo bench -p pptx-fast-core
cargo bench -p email-fast-core
cargo bench -p ooxml-archive

# One benchmark, by name
cargo bench -p xlsx-fast-core -- xlsx_200k_cells/json

# Compile only (what CI can afford)
cargo bench -p xlsx-fast-core --no-run --locked
```

criterion is a dev-dependency only, with its default features off. It adds 18
packages to `Cargo.lock` and none to a build of the crates themselves, and
`benches/` and `examples/` are excluded from the published packages.

Every input is built in memory and is the same on every run. Nothing is read
from a fixture file or the network. Before timing, each bench checks what it
parsed (cell, paragraph, slide, note and message counts), so a broken package
cannot look fast.

| Crate | Group | Input |
|---|---|---|
| `xlsx-fast-core` | `xlsx_200k_cells` | One sheet, 20,000 rows by 10 columns: a table of 10,000 shared strings (3,000 of them referenced), numbers, dates, formulas, booleans, inline strings. 8.9 MB of XML, 1.3 MB zipped, 15.1 MB of JSON. |
| `docx-fast-core` | `docx_2000_paragraphs` | 2,000 paragraphs of three runs, 100 tables, 200 footnotes, 80 comments, tracked insertions and deletions. 1.2 MB of XML, 2.0 MB of JSON. |
| `pptx-fast-core` | `pptx_200_slides` | 200 slides, each with a title, a five-paragraph body, a 4x4 table and a notes slide (603 parts). 0.8 MB of XML. |
| `email-fast-core` | `email_2mb_multipart`, `email_mbox_5000_messages`, `email_msg_fixture` | A 2.0 MB multipart message with plain and HTML bodies and two base64 attachments; a 2.0 MB mbox of 5,000 messages; an 8.7 kB `.msg` from the crate's fixture writer. |
| `ooxml-archive` | `archive_2000_entries_stored`, `archive_2000_entries_deflated` | 2,000 parts of about 4 kB each, 8.2 MB in all. |

Throughput is reported in bytes of uncompressed input per second. Items per
second follow from the same median and the counts above (for example 200,000
cells divided by the `json` time).

The `ndjson` and `arrow_ipc` benches of `xlsx-fast-core` call entry points
that take a path, so the generated workbook is written once to a temporary
file during setup.

### Wall-clock time is not enough on a busy machine

Criterion measures wall-clock time. On a machine that is doing other work the
same binary can differ by 40% or more between two runs, which hides any
change smaller than that. Two measures hold up much better and are what the
optimizations below were judged by:

- **Instructions retired** for one parse, from `/usr/bin/time -l` on macOS
  (`perf stat -e instructions` on Linux). Run to run it moves by about 1%.
- **User CPU time** summed over many parses in a loop.

Both need a one-shot driver: a process that parses one file, serializes the
result and exits. Each of `xlsx-fast-core`, `docx-fast-core`,
`pptx-fast-core` and `email-fast-core` has one in `examples/`, named
`<format>_bench_driver.rs` so that no two examples in the workspace share a name,
and one script runs them all on the benchmark inputs:

```bash
scripts/bench-instructions.sh        # three runs per input
scripts/bench-instructions.sh 5
```

It prints instructions retired, user CPU time and peak resident memory per
run (`/usr/bin/time -l` on macOS, `perf stat` on Linux). It measures and
never gates. Setting `PARSERS_BENCH_DUMP_INPUT=<path>` when running a bench
writes its generated input to that path, which is how the script gets the
files.

Compare builds made the same way. `cargo bench -p a -p b` and
`cargo bench -p a` can enable different features in shared dependencies, and
the two binaries are then not comparable.

## Baseline

Apple M1 (8 cores), 16 GB, macOS 26.5.2, rustc 1.98.1, release profile.
**The machine was under heavy load throughout** (load average between 25 and
150 from unrelated builds).

One parse plus JSON of each benchmark input as a whole process, start-up
included, from `scripts/bench-instructions.sh` (two runs):

| Input | Instructions retired | Peak resident memory |
|---|---|---|
| xlsx, 200,000 cells | 1.213e9, 1.219e9 | 51.2-51.3 MB |
| docx, 2,000 paragraphs | 125.6 M, 125.9 M | 11.9-13.9 MB |
| pptx, 200 slides | 143.0 M, 145.6 M | 9.1 MB |
| email, 2 MB multipart | 149.5 M, 156.4 M | 12.4-12.5 MB |

These were taken before the archive's part index became an ordered map,
which every input's part lookups go through. One run after that change gave
1.214e9, 126.2 M, 147.4 M and 157.1 M: within the spread above for xlsx, docx
and email, and 1% to 3% above it for pptx, which opens 603 parts. One run
does not settle whether that is the map or noise.

No criterion medians are recorded here. The ones taken while this file was
written came from a machine so loaded that two runs of the same binary
differed by 40% or more, and the code has changed since; a table of them
would be out of date and was never reproducible. Run `cargo bench` on a quiet
machine for throughput in MB/s. For scale only: on this machine, in a quiet
moment, one parse plus JSON of the 200,000-cell workbook took 0.16 s of wall
time as a whole process (about 1.2 M cells per second).

From reading the code, neither parser holds a decompressed part twice: a
part is inflated once into one buffer and read from it, and a UTF-16 part is
the only case that is transcoded into a second buffer.

### Where the time goes

Share of samples (macOS `sample`) in the benchmark named, taken before the
two optimizations listed further down. Shares for a pass or a stage include
what it calls; shares for a single function (`memmove`, malloc, inflate) are
its own time:

- **xlsx, `json`**: XML event reading (27%); JSON serialization (21%, of
  which string escaping 8%); cell value decoding (17%, of which date
  formatting 6%); attribute
  iteration and decoding (12%); inflate (5%). Counted across those, `memmove`
  was 12% of self time, as the cell vector and the output buffer grow.
- **docx, `json`**: the body pass (55%); a second full pass over the
  document part that only collects header and footer references (was 21%); JSON
  serialization (14%); notes (5%); inflate (2%). Across those, malloc and
  free were about 30% of all samples.
- **pptx, `json`**: XML event reading (15%); inflate (12%, of which 8% is
  building Huffman tables for 603 small parts); malloc and free (10%);
  copying (5%); attribute iteration (4%).
- **email, multipart**: stripping whitespace before base64 decoding, inside
  `mailparse` (33%); base64 encoding of attachment content (16%); MIME
  boundary search, inside `mailparse` (16%); base64 decoding (12%); JSON
  string escaping (8%).
- **email, mbox**: Latin-1 header decoding (8%); `mailparse` header parsing
  (7%); copying (6%); JSON string escaping (6%); line search (5%).
- **ooxml-archive, deflated**: inflate (35%); Huffman table building (16%);
  zeroing and copying buffers (20%); the decoder's read loop (5%).
- **ooxml-archive, stored**: local file header parsing in `rawzip` (17%);
  hashing part names with SipHash (14%); building the index (8%); part
  lookup (7%).

### Optimizations made with these benchmarks

| Change | Instructions retired | User CPU time |
|---|---|---|
| docx: borrow the element name and text of each XML event instead of copying them | 201.5 M to 152.0 M (-24.5%) | 100 parses: 2.29-2.38 s to 1.82-1.89 s (-20%) |
| xlsx: write a date cell's ISO text directly instead of through `strftime` | 1.337e9 to 1.262e9 (-5.6%) | 20 parses: 2.80-3.07 s to 2.62-2.91 s (-6%) |
| docx: find header and footer references while parsing the body, instead of in a pass of their own | 150.5 M to 122.6 M (-18%) | not measured |

The two docx changes are separate steps on the same document: the first took
it from 201.5 M to about 151 M instructions, the second from there to
122.6 M, which is 39% below where it started. These figures come from a
scratch driver, so they sit a little below the committed driver's in the
baseline table.

Peak memory did not change with any of them (xlsx about 51.5 MB, docx 11.8
to 13.2 MB before and after).

Looked at and left alone, because the gain was within the noise of the
measure used:

- xlsx: keeping the shared-string table as a `String` so a lookup does not
  re-validate UTF-8. `from_utf8` was 4% of samples, but the change saved only
  0.5% to 1% of instructions.
- xlsx: a fast path for attribute values that need no normalization. The
  existing path already borrows; an estimated 2% at most.

Known and not yet done:

- email: a third of a large message's time is `mailparse` filtering
  whitespace out of base64 one byte at a time. Separately, with
  `extract_attachment_content` an attachment is decoded and then encoded to
  base64 again for output (16% of the benchmark). Passing the original
  through is only the same output when it is canonical base64, which has to
  be checked byte for byte; not done yet.
- xlsx JSON: the workbook is serialized straight into the output, with no
  intermediate value. The `memmove` share is the output buffer and the cell
  vector doubling as they grow; the buffer belongs to the caller of
  `serde_json`, not to this crate.
- pptx and ooxml-archive: each part is inflated with a fresh decoder. Reusing
  one would save its set-up and zeroing, not the Huffman tables, which are
  built per deflate block; not measured yet.
- ooxml-archive: the part index is an ordered map, so that parts are walked
  in name order. Lookups in it only show for stored archives with thousands
  of parts, where the whole open takes about a millisecond; the effect of
  the ordered map on that benchmark has not been measured, and pptx may pay
  1% to 3% for it (see the baseline).

## What an optimization must keep

Speed never buys a change in output.

1. `tests/text_fidelity.rs`, `tests/xml_references.rs` and
   `tests/malformed_input_smoke.rs` pass in every crate that has them
   (`xlsx-fast-core`, `docx-fast-core`, `pptx-fast-core`; `ooxml-archive` has
   the last two; `email-fast-core` has `malformed_input_smoke.rs`), along
   with the rest of `cargo test -p <crate> --locked`.
2. Parsed output on real files is identical before and after. Parse a set of
   real `.xlsx`, `.docx`, `.pptx` and `.eml` files with the old and the new
   build and compare the JSON (and for xlsx the NDJSON and Arrow IPC bytes)
   file by file, byte for byte, including the error text of files that fail.
   Compare the old build with itself first. Builds from before the maps of
   docx, pptx and email (`metadata`, `settings`, raw headers) were written
   in key order give different bytes on every run, and can only be compared
   with object keys sorted.
3. The same input gives the same bytes every time, for JSON, NDJSON and
   Arrow IPC: `tests/deterministic_output.rs` in docx, pptx and email. A
   `HashMap` may hold data, but nothing is written in its iteration order.
4. `docx-fast-core/tests/header_footer_pass.rs` passes: the error a
   malformed document reports and the numbering of fields both follow the
   order header, footer, body, and a document that fails is not parsed a
   second time (the test counts parser runs, not time).
5. No `unsafe` is added, and nothing that depends on the input may
   `unwrap()` or `expect()`.
6. A claimed gain is larger than the run-to-run spread of the measure it was
   taken with, and that spread is stated next to it.
