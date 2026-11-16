//! The synthetic Turtle document shared by the benchmark and the profile example.
//!
//! Built in memory and fully deterministic: the same size always yields the
//! same bytes, so medians and output hashes are comparable between commits.

// The bench and the example each use a different subset of this module.
#![allow(dead_code)]

use std::fmt::Write as _;

/// Size of the benchmark document: 50 MB, under the default 64 MiB source budget.
pub const BENCH_DOCUMENT_BYTES: usize = 50 * 1000 * 1000;

/// Build a Turtle document of at least `target_bytes` bytes.
///
/// Every record exercises prefixed names, a long full IRI, plain, typed,
/// language-tagged and multi-line literals, a collection, an anonymous blank
/// node property list, a labelled blank node and a repeated statement. A
/// record is about 480 bytes and 13 distinct triples, so 50 MB stays well
/// under the default budget of two million statements.
pub fn synthetic_turtle(target_bytes: usize) -> Vec<u8> {
    let mut out = String::with_capacity(target_bytes + 1024);
    out.push_str(concat!(
        "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n",
        "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
        "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n",
        "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
        "@prefix dct: <http://purl.org/dc/terms/> .\n",
        "@prefix ex: <https://example.org/ns#> .\n",
        "@prefix exv: <https://example.org/ns/vocabulary/> .\n",
        "PREFIX data: <https://data.example.org/catalogue/2026/records/>\n",
        "@base <https://example.org/base/> .\n",
    ));
    let mut record = 0_u64;
    while out.len() < target_bytes {
        // `write!` to a `String` cannot fail.
        let _ = write!(
            out,
            concat!(
                "<https://data.example.org/catalogue/2026/records/department/{dept}/",
                "collection/long-lived-reference-data/item/{i:012}> a ex:Record , owl:Thing ;\n",
                "    rdfs:label \"Record number {i}\"@en , \"Registro {i}\"@pt-BR ;\n",
                "    ex:count \"{i}\"^^xsd:integer ; ex:weight 4.5e3 ; ex:active true ;\n",
                "    dct:description \"\"\"Line one of {i}\n    line two with a \\\"quote\\\"\"\"\" ;\n",
                "    exv:parts ( data:part-{i} \"x\" ) ;\n",
                "    ex:provenance [ dct:creator _:agent{agent} ; dct:created \"2026-01-{day:02}\"^^xsd:date ] ;\n",
                "    owl:sameAs <relative/{i}> , <urn:uuid:00000000-0000-4000-8000-{i:012x}> .\n",
                "data:part-{i} a ex:Record .\ndata:part-{i} a ex:Record .\n",
            ),
            dept = record % 97,
            i = record,
            agent = record % 1000,
            day = record % 28 + 1,
        );
        record += 1;
    }
    out.into_bytes()
}
