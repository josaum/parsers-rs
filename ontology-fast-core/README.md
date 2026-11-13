# ontology-fast-core

Canonical Turtle reader and indexed triple view.

Zero-copy parsing and pure provenance tracking over Turtle serializations, conforming to standard normative RDF 1.1 / 1.2 with strict fail-closed resource budgets.

## Limits and refusals

Every entry point (`parse_turtle`, `parse_turtle_terms`, `declared_prefixes`)
refuses a document that passes a limit with `OntologyError::ResourceLimit`,
which names the resource, the ceiling and what the document needed. Nothing is
truncated.

| Resource | Ceiling | Set by |
|---|---|---|
| `source_bytes` | 64 MiB | `OntologyLimits::max_source_bytes` |
| `statements` | 2,000,000 distinct statements | `OntologyLimits::max_statements` |
| `nesting_depth` | 10,000 brackets inside one another | `MAX_NESTING_DEPTH` (fixed) |
| `triple_term_depth` | 32 triple terms `<<( ... )>>` inside one another | `MAX_TRIPLE_TERM_DEPTH` (fixed) |
| `term_bytes` | the larger of 256 MiB and 64 bytes per source byte; `parse_turtle` and `parse_turtle_terms` only | `TERM_BYTES_FLOOR`, `TERM_BYTES_PER_SOURCE_BYTE` (fixed) |

`nesting_depth` counts `[`, `(`, `<<` and `{|` that are open at once. Earlier
releases had no such limit: a document nested deeper than 10,000 levels used to
read and is now refused. The check is a scan of the bytes that runs before the
parser. It never refuses a valid document inside the limit, but a document that
is malformed ahead of a long run of brackets can be refused for `nesting_depth`
where it used to be refused as `syntax`.

`term_bytes` counts the text of every term the parser emits, after prefixes and
the base are expanded and including statements that repeat an earlier one. It
exists because a prefixed name is a few bytes that expand to a whole namespace:
without it, one megabyte-long namespace and a few hundred names cost more than
a gigabyte, and a few hundred thousand did not finish. `parse_turtle` applies
the same ceiling to the text it renders, since a long prefix name expands a
short IRI the same way. `declared_prefixes` is not charged: it keeps no term,
and its peak resident memory is the document plus about 3 MiB (12 MiB for a
9 MiB document that expands 37 times, 23 MiB for a 20 MiB one).

Measured expansion: 63 real Turtle files expand at most 4.73 times (median
2.86). The worst ordinary shape known is one subject with one long object list
of prefixed names under a namespace of about 120 bytes, which expands 37 to 39
times. So only a document under 4 MiB whose terms pass 256 MiB, or a larger one
that expands more than 64 times, is refused. The budget bounds the text the
parser copies, not resident memory, which depends on how much of that text is
distinct; the table below has measured peaks.

### What the limits cost in memory

Peak resident memory, release build, default limits, measured on macOS/arm64:

| Document | `parse_turtle` | `parse_turtle_terms` |
|---|---|---|
| 1 MB: a 1 MiB namespace, 84 statements (just inside `term_bytes`) | 267 MiB | 267 MiB |
| the same, with the prefix rebound at the end so nothing abbreviates | 777 MiB | 267 MiB |
| 3 MB: a collection of 1,000,000 `[]` (refused, `statements`) | 795 MiB | 1291 MiB |
| 2 MB: a collection of 1,000,000 `1` (refused, `statements`) | 483 MiB | 950 MiB |
| 9 MiB: one object list under a 120-byte namespace, expands 37 times | 445 MiB | 648 MiB |
| 16 MiB: the same under a 203-byte namespace, expands 60 times (just inside `term_bytes`) | 845 MiB | 1438 MiB |

The collection rows are the price of the statement budget, not of expansion:
each item is two statements, and a row of the term view owns three strings.
The term view can therefore pass 1 GiB resident from a 3 MB document before
the statement budget refuses it. A caller that reads untrusted input and needs
a lower ceiling should lower `max_statements`; memory scales with it.
