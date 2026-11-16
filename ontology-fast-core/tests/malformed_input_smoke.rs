//! Malformed Turtle must be rejected with an error, never a panic.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use ontology_fast_core::{
    declared_prefixes, parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits,
    MAX_TRIPLE_TERM_DEPTH,
};

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

/// A statement whose object is `depth` triple terms inside one another.
fn nested_triple_terms(depth: usize) -> Vec<u8> {
    let mut document = b"<http://e/s> <http://e/p> ".to_vec();
    document.extend(smoke::nested(
        b"<<( <http://e/s> <http://e/p> ",
        b"1",
        b" )>>",
        depth,
    ));
    document.extend_from_slice(b" .\n");
    document
}

/// Every entry point's outcome for `document`, read on a small stack.
fn outcomes_on_a_small_stack(document: Vec<u8>) -> [Result<usize, OntologyError>; 3] {
    let (send, receive) = std::sync::mpsc::channel();
    smoke::run_on_small_stack(move || {
        let limits = OntologyLimits::default();
        let outcomes = [
            parse_turtle(&document, limits).map(|read| read.triples().len()),
            parse_turtle_terms(&document, limits).map(|read| read.len()),
            declared_prefixes(&document, limits).map(|read| read.len()),
        ];
        send.send(outcomes).expect("the receiver is alive");
    });
    receive.recv().expect("the thread sent its outcomes")
}

#[test]
fn triple_terms_nested_to_the_limit_read_on_a_small_stack() {
    // The parser builds the whole nested term and it is taken apart again;
    // the two text views then refuse it as the triple term it is.
    let refused = Err(OntologyError::TripleTerm { position: "object" });
    assert_eq!(
        outcomes_on_a_small_stack(nested_triple_terms(MAX_TRIPLE_TERM_DEPTH)),
        [refused.clone(), refused, Ok(0)]
    );
}

#[test]
fn triple_terms_nested_past_the_limit_are_refused_on_a_small_stack() {
    // One past the limit, and the depth the bracket limit used to admit,
    // which overflowed a thread four times this size.
    for depth in [MAX_TRIPLE_TERM_DEPTH + 1, 5_000] {
        let refused = Err(OntologyError::ResourceLimit {
            resource: "triple_term_depth",
            limit: MAX_TRIPLE_TERM_DEPTH,
            observed: MAX_TRIPLE_TERM_DEPTH + 1,
        });
        assert_eq!(
            outcomes_on_a_small_stack(nested_triple_terms(depth)),
            [refused.clone(), refused.clone(), refused],
            "{depth} deep"
        );
    }
}

#[test]
fn reified_triples_and_annotations_do_not_cost_stack_per_level() {
    // Neither form is a term inside a term: each level is a statement about a
    // reifier, so four thousand of them need no more stack than one.
    let depth = 4_000;
    let reified = format!(
        "{}<http://e/s>{} <http://e/p> 1 .",
        "<< ".repeat(depth),
        " <http://e/p> 1 >>".repeat(depth)
    );
    let annotated = format!(
        "<http://e/s> <http://e/p> 1 {}{} .",
        "{| <http://e/p> 1 ".repeat(depth),
        "|}".repeat(depth)
    );
    for document in [reified, annotated] {
        let [_, _, prefixes] = outcomes_on_a_small_stack(document.into_bytes());
        assert_eq!(prefixes, Ok(0));
    }
}
