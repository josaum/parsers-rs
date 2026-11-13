//! Every field of `OntologyLimits` is enforced exactly at its boundary.
//!
//! For each budget: a document that needs exactly the limit reads, and one
//! that needs one more is the documented `ResourceLimit` refusal, carrying the
//! configured ceiling and what the document needed.

use ontology_fast_core::{
    declared_prefixes, parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits,
    MAX_NESTING_DEPTH, TERM_BYTES_FLOOR, TERM_BYTES_PER_SOURCE_BYTE,
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

#[test]
fn rendering_does_not_test_every_prefix_against_every_term() {
    // Forty thousand prefixes and one hundred and twenty thousand distinct
    // terms: testing each term against each prefix is billions of string
    // comparisons, where a sorted lookup is a few million. What is asserted
    // here is the answer; the cost is asserted as a count of comparisons and
    // links, not on a clock, by the unit test
    // `a_namespace_lookup_is_logarithmic_in_the_prefixes_for_every_shape`.
    let count = 40_000;
    let mut text = String::new();
    for n in 0..count {
        text.push_str(&format!("@prefix p{n}: <urn:n:{n}/> .\n"));
    }
    for n in 0..count {
        text.push_str(&format!("p{n}:s <urn:n:{n}/p> <urn:q:{n}> .\n"));
    }
    let ontology = parse_turtle(text.as_bytes(), OntologyLimits::default()).expect("parses");
    assert_eq!(ontology.triples().len(), count);
    assert_eq!(ontology.triples()[7].to_line(), "p7:s\tp7:p\t<urn:q:7>");
}

#[test]
fn rendering_does_not_search_once_per_namespace_that_nearly_matches() {
    // A thousand namespaces that each share one byte more with every IRI than
    // the one before, and none of which starts any IRI. A lookup that drops
    // one candidate per search makes a thousand searches per term; one search
    // and a comparison of lengths is enough. The answer is asserted here and
    // the cost, as a count, by the unit test named above.
    let prefixes = 1_000;
    let rows = 5_000;
    let mut text = String::new();
    for k in 0..prefixes {
        text.push_str(&format!("@prefix p{k}: <a:{}a> .\n", "b".repeat(k)));
    }
    text.push_str(&format!("@base <a:{}/> .\n", "b".repeat(prefixes + 1)));
    for n in 0..rows {
        text.push_str(&format!("<x{n}> <p> <o> .\n"));
    }
    let ontology = parse_turtle(text.as_bytes(), OntologyLimits::default()).expect("parses");
    assert_eq!(ontology.triples().len(), rows);
    assert_eq!(
        ontology.triples()[7].subject,
        format!("<a:{}/x7>", "b".repeat(prefixes + 1))
    );
}

#[test]
fn a_chain_of_nested_namespaces_renders_through_the_longest_that_fits() {
    // Each namespace starts the next. An IRI is rendered through the longest
    // one that starts it, wherever along the chain that is.
    let prefixes = 300;
    let mut text = String::new();
    for k in 1..=prefixes {
        text.push_str(&format!("@prefix p{k}: <http://e/{}> .\n", "a".repeat(k)));
    }
    let deep = "a".repeat(prefixes);
    text.push_str(&format!(
        "<http://e/ab> <http://e/{deep}b> <http://e/{}c> .\n",
        "a".repeat(17)
    ));
    let ontology = parse_turtle(text.as_bytes(), OntologyLimits::default()).expect("parses");
    assert_eq!(ontology.triples()[0].to_line(), "p1:b\tp300:b\tp17:c");
}

#[test]
fn nesting_passes_at_the_limit_and_refuses_one_level_more() {
    let nested = |open: &str, close: &str, depth: usize| {
        format!(
            "<a:a> <a:b> {}1{} .",
            open.repeat(depth),
            close.repeat(depth)
        )
        .into_bytes()
    };
    for (open, close) in [("(", ")"), ("[<a:b> ", "]")] {
        let at_limit = nested(open, close, MAX_NESTING_DEPTH);
        let over_limit = nested(open, close, MAX_NESTING_DEPTH + 1);
        for (name, entry) in ENTRIES {
            assert!(
                entry(&at_limit, OntologyLimits::default()).is_ok(),
                "{name}: {open:?} nested to the limit"
            );
            assert_eq!(
                entry(&over_limit, OntologyLimits::default()),
                Err(OntologyError::ResourceLimit {
                    resource: "nesting_depth",
                    limit: MAX_NESTING_DEPTH,
                    observed: MAX_NESTING_DEPTH + 1,
                }),
                "{name}: {open:?} nested one level past the limit"
            );
        }
    }
}

/// Whether `outcome` is the expanded-text refusal for a source of `source_bytes`.
fn refuses_term_bytes(outcome: &Result<usize, OntologyError>, source_bytes: usize) -> bool {
    let budget = TERM_BYTES_FLOOR.max(source_bytes * TERM_BYTES_PER_SOURCE_BYTE);
    matches!(
        outcome,
        Err(OntologyError::ResourceLimit { resource: "term_bytes", limit, observed })
            if *limit == budget && *observed > budget
    )
}

#[test]
fn text_expanded_far_beyond_the_source_is_refused_where_terms_are_kept() {
    // One megabyte of namespace, written once, is copied into every term that
    // uses it: about a megabyte of source expands to several hundred.
    let long = "x".repeat(1024 * 1024);
    let mut distinct = format!("@prefix p: <a:{long}> .\n");
    let mut repeated = distinct.clone();
    for n in 0..120 {
        distinct.push_str(&format!("p:s{n} p:p{n} p:o{n} .\n"));
        // The same statement every time: nothing is retained, yet every copy
        // is still made, so duplicates count.
        repeated.push_str("p:a p:a p:a .\n");
    }
    // No prefix at all: one long subject, repeated by the object list.
    let mut listed = format!("<a:{long}> <a:p> 0");
    for n in 1..400 {
        listed.push_str(&format!(",{n}"));
    }
    listed.push_str(" .");
    // A long base, resolved into every relative IRI.
    let mut based = format!("@base <a:{long}/> .\n");
    for n in 0..120 {
        based.push_str(&format!("<s{n}> <p{n}> <o{n}> .\n"));
    }
    for (shape, text, declared) in [
        ("distinct", distinct, 1),
        ("repeated", repeated, 1),
        ("listed", listed, 0),
        ("based", based, 0),
    ] {
        for (name, entry) in ENTRIES {
            let outcome = entry(text.as_bytes(), OntologyLimits::default());
            if name == "declared_prefixes" {
                // It keeps no term, so term text over the parsing budget is
                // nothing it has to refuse.
                assert_eq!(outcome, Ok(declared), "{name} on the {shape} document");
                continue;
            }
            assert!(
                refuses_term_bytes(&outcome, text.len()),
                "{name} on the {shape} document: {:?}",
                outcome.map_err(|error| error.to_string())
            );
        }
    }
}

#[test]
fn a_long_object_list_under_an_ordinary_namespace_reads() {
    // The worst ordinary shape known: one subject, one object list of
    // prefixed names, a namespace of about 120 bytes. Every item repeats the
    // subject and the predicate in full, so 9 MiB expand about 37 times, past
    // the 256 MiB floor and past 32 bytes per source byte.
    let namespace = format!("http://data.example.org/{}/", "catalogue/".repeat(9));
    let mut text = format!("@prefix x: <{namespace}> .\nx:s x:p x:o");
    let mut items = 1;
    while text.len() < 9 * 1024 * 1024 {
        text.push_str(&format!(", x:{items}"));
        items += 1;
    }
    text.push_str(" .\n");
    let expanded = items * 3 * (namespace.len() + 1);
    assert!(expanded > TERM_BYTES_FLOOR && expanded > 32 * text.len());
    for (name, entry) in ENTRIES {
        let expected = if name == "declared_prefixes" {
            1
        } else {
            items
        };
        assert_eq!(
            entry(text.as_bytes(), OntologyLimits::default()),
            Ok(expected),
            "{name}"
        );
    }
}

#[test]
fn text_expanded_within_the_allowance_still_reads() {
    // The same shape, held under the floor: 60 statements of three terms of
    // one megabyte each is 180 MiB.
    let long = "x".repeat(1024 * 1024);
    let mut text = format!("@prefix p: <a:{long}> .\n");
    for n in 0..60 {
        text.push_str(&format!("p:s{n} p:p{n} p:o{n} .\n"));
    }
    for (name, entry) in ENTRIES {
        let expected = if name == "declared_prefixes" { 1 } else { 60 };
        assert_eq!(
            entry(text.as_bytes(), OntologyLimits::default()),
            Ok(expected),
            "{name}"
        );
    }
}

#[test]
fn a_long_prefix_name_cannot_expand_the_rendered_text_past_the_allowance() {
    // The other direction: a short IRI rendered through a prefix whose name
    // is a megabyte long. The terms are small; the text form is not.
    let long = "x".repeat(1024 * 1024);
    let mut text = format!("@prefix {long}: <a:> .\n");
    for n in 0..120 {
        text.push_str(&format!("<a:s{n}> <a:p{n}> <a:o{n}> .\n"));
    }
    let outcome =
        parse_turtle(text.as_bytes(), OntologyLimits::default()).map(|read| read.triples().len());
    assert!(
        refuses_term_bytes(&outcome, text.len()),
        "{:?}",
        outcome.map_err(|error| error.to_string())
    );
    assert_eq!(
        parse_turtle_terms(text.as_bytes(), OntologyLimits::default()).map(|read| read.len()),
        Ok(120)
    );
}

#[test]
fn the_nesting_guard_may_answer_before_a_syntax_error() {
    // Malformed before its brackets: the fourth quote ends the long string
    // early, and the parser alone would stop there with a syntax error. The
    // guard scans bytes before the parser runs, so this document is refused
    // for its nesting instead. It is refused either way.
    let text = format!(
        "<a:a> <a:b> \"\"\"x\"\"\"\" , \"{}\" .",
        "(".repeat(MAX_NESTING_DEPTH + 1)
    );
    for (name, entry) in ENTRIES {
        assert_eq!(
            entry(text.as_bytes(), OntologyLimits::default()),
            Err(OntologyError::ResourceLimit {
                resource: "nesting_depth",
                limit: MAX_NESTING_DEPTH,
                observed: MAX_NESTING_DEPTH + 1,
            }),
            "{name}"
        );
        // One bracket fewer and the parser reports what it always did.
        let within = text.replacen('(', "", 1);
        assert!(
            matches!(
                entry(within.as_bytes(), OntologyLimits::default()),
                Err(OntologyError::Syntax(_))
            ),
            "{name}"
        );
    }
}
