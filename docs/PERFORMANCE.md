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
`pptx-fast-core` and `email-fast-core` has one in `examples/bench_driver.rs`,
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
150 from unrelated builds), so the criterion medians below are slower than an
idle machine would give and carry the noise shown. They are a floor to
improve on, not a target.

Criterion medians, with the 95% interval criterion reported. Each row is one
run. The docx rows are this branch (the faster of two runs; the other gave
37.1 ms and 31.7 ms while a compile was running beside it). The xlsx rows
were taken with the build before the date change, which wall-clock time
cannot tell apart from this branch. The pptx, email and archive crates have
no source change.

| Benchmark | Median | Interval | Throughput | Items per second |
|---|---|---|---|---|
| `xlsx_200k_cells/json` | 390 ms | 373-408 ms | 21.8 MiB/s | 0.51 M cells/s |
| `xlsx_200k_cells/parse_only` | 263 ms | 235-293 ms | 32.3 MiB/s | 0.76 M cells/s |
| `xlsx_200k_cells/ndjson` | 263 ms | 236-292 ms | 32.3 MiB/s | 0.76 M cells/s |
| `xlsx_200k_cells/arrow_ipc` | 252 ms | 213-292 ms | 33.7 MiB/s | 0.79 M cells/s |
| `docx_2000_paragraphs/json` | 22.6 ms | 21.3-24.0 ms | 52.0 MiB/s | 88,000 paragraphs/s |
| `docx_2000_paragraphs/parse_only` | 15.8 ms | 14.8-17.1 ms | 74.2 MiB/s | 126,000 paragraphs/s |
| `pptx_200_slides/json` | 14.8 ms | 13.6-16.5 ms | 52.9 MiB/s | 13,500 slides/s |
| `pptx_200_slides/parse_only` | 12.1 ms | 11.9-12.4 ms | 64.8 MiB/s | 16,500 slides/s |
| `email_2mb_multipart/json` | 11.6 ms | 11.2-12.1 ms | 165 MiB/s | 86 messages/s |
| `email_2mb_multipart/json_metadata_only` | 12.6 ms | 11.5-13.6 ms | 152 MiB/s | 79 messages/s |
| `email_mbox_5000_messages/ndjson` | 60.3 ms | 55.4-65.8 ms | 32.2 MiB/s | 83,000 messages/s |
| `email_msg_fixture/json` | 23.7 µs | 21.1-26.5 µs | 351 MiB/s | 42,000 messages/s |
| `archive_2000_entries_stored/open` | 0.84 ms | 0.75-0.93 ms | 9.2 GiB/s | 2.4 M parts/s |
| `archive_2000_entries_stored/open_and_read_all` | 1.39 ms | 1.20-1.62 ms | 5.5 GiB/s | 1.4 M parts/s |
| `archive_2000_entries_deflated/open` | 0.56 ms | 0.50-0.63 ms | 13.7 GiB/s | 3.6 M parts/s |
| `archive_2000_entries_deflated/open_and_read_all` | 78.5 ms | 68.7-88.6 ms | 100 MiB/s | 25,500 parts/s |

The xlsx rows were measured at the highest load: one parse plus JSON of the
same workbook as a whole process, start-up included, took 0.16 s of wall
time and 0.14 s of user CPU time when the machine had a quiet moment
(about 1.4 M cells/s).

One parse plus JSON as a whole process:

| Input | Instructions retired | Peak resident memory |
|---|---|---|
| xlsx, 200,000 cells | 1.262e9 | 51.5 MB |
| docx, 2,000 paragraphs | 122.6 M | 11.8-12.9 MB |

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

The docx criterion rows in the table above were taken before the last
change.

Peak memory did not change with either (xlsx 51.5 MB, docx 11.8-13.2 MB
before and after).

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
- ooxml-archive: the part index is a `HashMap` with SipHash. It only shows
  for stored archives with thousands of parts, where the whole open takes
  under a millisecond.

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
   order header, footer, body.
5. No `unsafe` is added, and nothing that depends on the input may
   `unwrap()` or `expect()`.
6. A claimed gain is larger than the run-to-run spread of the measure it was
   taken with, and that spread is stated next to it.
