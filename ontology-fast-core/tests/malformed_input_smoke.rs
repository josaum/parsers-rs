//! Malformed Turtle must be rejected with an error, never a panic.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use ontology_fast_core::{parse_turtle, parse_turtle_terms, OntologyLimits};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

fn seed() -> Vec<u8> {
    concat!(
        "@prefix ex: <http://example.org/> .\n",
        "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
        "@base <http://example.org/base/> .\n",
        "ex:a a ex:C ; ex:p \"lit\"@en , \"3\"^^xsd:integer , 4.5e3 , true ;\n",
        "  ex:q [ ex:r ( 1 2 ex:b ) ] .\n",
        "<rel> ex:p \"\"\"multi\nline \\u00e9 \\\"q\\\"\"\"\" .\n",
        "_:b1 ex:p _:b2 . # comment\n",
    )
    .as_bytes()
    .to_vec()
}

#[test]
fn mutated_turtle_never_panics() {
    smoke::assert_no_panics("ontology-fast-core turtle", &[seed()], 20_000, |bytes| {
        let _ = parse_turtle(bytes, OntologyLimits::default());
        let _ = parse_turtle_terms(bytes, OntologyLimits::default());
    });
}

#[test]
fn deeply_nested_turtle_is_bounded_on_a_small_stack() {
    for (open, close) in [(&b"[ ex:p "[..], &b" ]"[..]), (&b"( "[..], &b" )"[..])] {
        let mut document = b"@prefix ex: <http://example.org/> .\nex:a ex:p ".to_vec();
        document.extend(smoke::nested(open, b"ex:b", close, 100_000));
        document.extend_from_slice(b" .\n");
        smoke::run_on_small_stack(move || {
            let _ = parse_turtle(&document, OntologyLimits::default());
        });
    }
}
