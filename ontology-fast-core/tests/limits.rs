//! Every field of `OntologyLimits` is enforced exactly at its boundary.
//!
//! For each budget: a document that needs exactly the limit reads, and one
//! that needs one more is the documented `ResourceLimit` refusal, carrying the
//! configured ceiling and what the document needed.

use ontology_fast_core::{
    declared_prefixes, parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits,
};

type Entry = fn(&[u8], OntologyLimits) -> Result<usize, OntologyError>;

const ENTRIES: [(&str, Entry); 3] = [
    ("parse_turtle", |source, limits| {
        parse_turtle(source, limits).map(|ontology| ontology.triples().len())
    }),
    ("parse_turtle_terms", |source, limits| {
        parse_turtle_terms(source, limits).map(|triples| triples.len())
    }),
    ("declared_prefixes", |source, limits| {
        declared_prefixes(source, limits).map(|prefixes| prefixes.len())
    }),
];

/// `count` distinct triples, each written twice so duplicates cannot count.
fn distinct_triples(count: usize) -> Vec<u8> {
    let mut text = String::from("@prefix ex: <urn:x:> .\n");
    for n in 0..count {
        text.push_str(&format!("ex:s ex:p {n} .\nex:s ex:p {n} .\n"));
    }
    text.into_bytes()
}

#[test]
fn max_source_bytes_passes_at_the_limit_and_refuses_one_byte_more() {
    let source = distinct_triples(5);
    for (name, entry) in ENTRIES {
        let at_limit = OntologyLimits::with_overrides(Some(source.len()), None);
        assert!(entry(&source, at_limit).is_ok(), "{name} at the limit");

        let limit = source.len() - 1;
        assert_eq!(
            entry(&source, OntologyLimits::with_overrides(Some(limit), None)),
            Err(OntologyError::ResourceLimit {
                resource: "source_bytes",
                limit,
                observed: source.len(),
            }),
            "{name} one byte over"
        );
    }
}

#[test]
fn max_source_bytes_is_checked_before_the_document_is_read() {
    // Not Turtle and not UTF-8: only a check made before parsing reports the
    // budget rather than a syntax refusal.
    let source = [0xff_u8; 9];
    for (name, entry) in ENTRIES {
        assert_eq!(
            entry(&source, OntologyLimits::with_overrides(Some(8), None)),
            Err(OntologyError::ResourceLimit {
                resource: "source_bytes",
                limit: 8,
                observed: 9,
            }),
            "{name}"
        );
        assert!(
            matches!(
                entry(&source, OntologyLimits::with_overrides(Some(9), None)),
                Err(OntologyError::Syntax(_))
            ),
            "{name} at the limit reaches the parser"
        );
    }
}

#[test]
fn max_statements_passes_at_the_limit_and_refuses_one_statement_more() {
    for count in [1_usize, 2, 17, 1000] {
        let source = distinct_triples(count);
        for (name, entry) in &ENTRIES[..2] {
            assert_eq!(
                entry(&source, OntologyLimits::with_overrides(None, Some(count))),
                Ok(count),
                "{name}: {count} distinct statements fit a budget of {count}"
            );
            assert_eq!(
                entry(
                    &source,
                    OntologyLimits::with_overrides(None, Some(count - 1))
                ),
                Err(OntologyError::ResourceLimit {
                    resource: "statements",
                    limit: count - 1,
                    observed: count,
                }),
                "{name}: {count} distinct statements exceed a budget of {}",
                count - 1
            );
        }
    }
}

#[test]
fn max_statements_counts_full_terms_not_rendered_text() {
    // Two distinct statements whose text form is the same row: the budget is
    // spent on what the document states, so both views refuse alike.
    let source = b"@prefix ex: <urn:x:> .\nex:s ex:p ex:o , \"ex:o\" .\n";
    for (name, entry) in &ENTRIES[..2] {
        assert_eq!(
            entry(source, OntologyLimits::with_overrides(None, Some(1))),
            Err(OntologyError::ResourceLimit {
                resource: "statements",
                limit: 1,
                observed: 2,
            }),
            "{name}"
        );
    }
    assert_eq!(
        parse_turtle(source, OntologyLimits::with_overrides(None, Some(2)))
            .map(|ontology| ontology.triples().len()),
        Ok(1),
        "the text view folds the two statements into one row"
    );
}

#[test]
fn max_statements_does_not_apply_to_the_prefix_table() {
    let source = distinct_triples(3);
    assert_eq!(
        declared_prefixes(&source, OntologyLimits::with_overrides(None, Some(0)))
            .map(|prefixes| prefixes.len()),
        Ok(1),
        "the prefix table retains no statement"
    );
}
