//! Reaching a budget, or sitting just inside it, costs bounded memory.
//!
//! The capped allocator aborts this binary once live memory passes its cap, so
//! each case below is a proof that the reader stayed under it.
//!
//! The cases that spend a whole default budget parse two million statements,
//! which takes minutes in an unoptimized build, so they are ignored by default:
//!
//! ```text
//! cargo test --release -p ontology-fast-core --test limits_memory -- --ignored
//! ```
//!
//! They run one after another inside a single test: the cap counts the whole
//! process, and two heavy documents alive at once would be charged together.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use std::fmt::Write as _;

use ontology_fast_core::{
    declared_prefixes, parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits,
    MAX_NESTING_DEPTH,
};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

/// `count` distinct triples in as few source bytes as Turtle allows.
fn short_triples(count: usize) -> Vec<u8> {
    let mut text = String::from("<a:a> <a:b> 0");
    for n in 1..count {
        // `write!` to a `String` cannot fail.
        let _ = write!(text, ",{n}");
    }
    text.push('.');
    text.into_bytes()
}

fn statements_refusal(limits: OntologyLimits) -> OntologyError {
    OntologyError::ResourceLimit {
        resource: "statements",
        limit: limits.max_statements,
        observed: limits.max_statements + 1,
    }
}

#[test]
#[ignore = "parses two million statements; minutes unless built with --release"]
fn the_default_budgets_bound_memory_at_and_past_each_limit() {
    let limits = OntologyLimits::default();

    // max_statements, exactly at the default: the most rows a result can hold.
    let at_limit = short_triples(limits.max_statements);
    assert_eq!(
        parse_turtle(&at_limit, limits).map(|ontology| ontology.triples().len()),
        Ok(limits.max_statements)
    );
    assert_eq!(
        parse_turtle_terms(&at_limit, limits).map(|triples| triples.len()),
        Ok(limits.max_statements)
    );
    drop(at_limit);

    // One statement past it: refused, having retained no more than the budget.
    let over_limit = short_triples(limits.max_statements + 1);
    assert_eq!(
        parse_turtle(&over_limit, limits).map(|ontology| ontology.triples().len()),
        Err(statements_refusal(limits))
    );
    assert_eq!(
        parse_turtle_terms(&over_limit, limits).map(|triples| triples.len()),
        Err(statements_refusal(limits))
    );
    drop(over_limit);

    // The same budget spent on blank nodes: a collection is two statements
    // and one fresh blank node per two source bytes.
    let mut collection = b"<a:a> <a:b> (".to_vec();
    collection.extend(b"1 ".repeat(limits.max_statements / 2 - 1));
    collection.extend_from_slice(b").");
    assert_eq!(
        parse_turtle(&collection, limits).map(|ontology| ontology.triples().len()),
        Ok(limits.max_statements - 1)
    );
    assert_eq!(
        parse_turtle_terms(&collection, limits).map(|triples| triples.len()),
        Ok(limits.max_statements - 1)
    );
    drop(collection);

    // max_source_bytes, exactly at the default, spent on one literal.
    let mut literal = b"<a:a> <a:b> \"".to_vec();
    literal.resize(limits.max_source_bytes - 3, b'x');
    literal.extend_from_slice(b"\" .");
    assert_eq!(literal.len(), limits.max_source_bytes);
    assert_eq!(
        parse_turtle(&literal, limits).map(|ontology| ontology.triples().len()),
        Ok(1)
    );
    assert_eq!(
        parse_turtle_terms(&literal, limits).map(|triples| triples.len()),
        Ok(1)
    );
}

#[test]
fn one_byte_past_the_default_source_budget_is_refused_before_it_is_copied() {
    let limits = OntologyLimits::default();
    let literal = vec![b'x'; limits.max_source_bytes + 1];
    for refused in [
        parse_turtle(&literal, limits).map(|_| ()),
        parse_turtle_terms(&literal, limits).map(|_| ()),
        declared_prefixes(&literal, limits).map(|_| ()),
    ] {
        assert_eq!(
            refused,
            Err(OntologyError::ResourceLimit {
                resource: "source_bytes",
                limit: limits.max_source_bytes,
                observed: limits.max_source_bytes + 1,
            })
        );
    }
}

#[test]
fn unclosed_brackets_filling_the_source_budget_are_refused_before_parsing() {
    // Opening brackets cost the parser state and no statement, so only the
    // nesting limit stands between this document and the allocator's cap.
    let limits = OntologyLimits::default();
    for open in [&b"("[..], b"[<a:b> ", b"<<", b"<a:c> {| <a:b> "] {
        let mut source = b"<a:a> <a:b> ".to_vec();
        while source.len() + open.len() <= limits.max_source_bytes {
            source.extend_from_slice(open);
        }
        for refused in [
            parse_turtle(&source, limits).map(|_| ()),
            parse_turtle_terms(&source, limits).map(|_| ()),
            declared_prefixes(&source, limits).map(|_| ()),
        ] {
            assert_eq!(
                refused,
                Err(OntologyError::ResourceLimit {
                    resource: "nesting_depth",
                    limit: MAX_NESTING_DEPTH,
                    observed: MAX_NESTING_DEPTH + 1,
                })
            );
        }
    }
}
