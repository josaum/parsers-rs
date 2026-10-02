//! Reader contract: prefix handling, blank node labels, and every refusal.
//!
//! The corpus test pins what the live modules read to. These cases pin what the
//! live modules do not exercise yet: a directive the grammar allows but no
//! module uses, and each way the reader refuses a document.

use ontology_fast_core::{
    parse_turtle, parse_turtle_terms, OntologyError, OntologyLimits, TermView, TripleText,
};

#[expect(
    clippy::expect_used,
    reason = "test helper: a panic here is the test failure report"
)]
fn lines(source: &str) -> Vec<String> {
    parse_turtle(source.as_bytes(), OntologyLimits::default())
        .expect("document parses")
        .triples()
        .iter()
        .map(TripleText::to_line)
        .collect()
}

#[test]
fn a_sparql_style_prefix_directive_abbreviates_like_an_at_prefix() {
    // Turtle 1.1 §6.5 `sparqlPrefix`: keyword case-insensitive, no trailing dot.
    for keyword in ["PREFIX", "prefix", "PrEfIx"] {
        let source =
            format!("{keyword} atlas: <https://atlas.example/ns#>\natlas:ROAS a atlas:Metric .\n");
        assert_eq!(
            lines(&source),
            vec!["atlas:ROAS\ta\tatlas:Metric".to_owned()],
            "{keyword}"
        );
        let ontology =
            parse_turtle(source.as_bytes(), OntologyLimits::default()).expect("document parses");
        assert_eq!(
            ontology.prefixes().get("atlas").map(String::as_str),
            Some("https://atlas.example/ns#")
        );
        assert_eq!(ontology.index().by_subject("atlas:ROAS").len(), 1);
    }
}

#[test]
fn a_directive_looking_line_inside_a_long_literal_declares_nothing() {
    let source = "@prefix atlas: <https://atlas.example/ns#> .\n\
                  atlas:check atlas:query \"\"\"\n\
                  @prefix bogus: <https://atlas.example/ns#RO> .\n\
                  PREFIX other: <https://atlas.example/> \n\
                  \"\"\" .\n\
                  atlas:ROAS a atlas:Metric .\n";
    let ontology =
        parse_turtle(source.as_bytes(), OntologyLimits::default()).expect("document parses");
    let declared: Vec<&str> = ontology.prefixes().keys().map(String::as_str).collect();
    assert_eq!(declared, vec!["atlas"], "only the real directive declares");
    let rendered: Vec<String> = ontology.triples().iter().map(TripleText::to_line).collect();
    assert!(
        rendered.contains(&"atlas:ROAS\ta\tatlas:Metric".to_owned()),
        "a longer namespace minted from literal text must not re-abbreviate: {rendered:?}"
    );
}

#[test]
fn the_empty_prefix_and_a_directive_sharing_a_line_are_both_declared() {
    let source = "@prefix : <urn:a:> . @prefix b: <urn:b:> .\n:s b:p :o .\n";
    assert_eq!(lines(source), vec![":s\tb:p\t:o".to_owned()]);
}

#[test]
fn a_redefined_prefix_renders_through_its_final_binding_only() {
    // `ex:` names two namespaces in turn. The text form carries one prefix
    // table, so a prefixed name must expand to one IRI: the final binding is
    // used, and a term from the superseded namespace stays a full IRI.
    let source = "@prefix ex: <urn:first:> .\n\
                  ex:a ex:p ex:b .\n\
                  @prefix ex: <urn:second:> .\n\
                  ex:a ex:p ex:b .\n";
    let ontology =
        parse_turtle(source.as_bytes(), OntologyLimits::default()).expect("document parses");
    assert_eq!(
        ontology.prefixes().get("ex").map(String::as_str),
        Some("urn:second:")
    );
    let rendered: Vec<String> = ontology.triples().iter().map(TripleText::to_line).collect();
    assert_eq!(
        rendered,
        vec![
            "<urn:first:a>\t<urn:first:p>\t<urn:first:b>".to_owned(),
            "ex:a\tex:p\tex:b".to_owned(),
        ]
    );
}

#[test]
fn a_prefix_declared_after_its_first_use_still_abbreviates_the_whole_document() {
    let source = "<urn:a:s> <urn:a:p> <urn:a:o> .\n@prefix a: <urn:a:> .\n";
    assert_eq!(lines(source), vec!["a:s\ta:p\ta:o".to_owned()]);
}

#[test]
fn the_longest_declared_namespace_wins_and_ties_break_by_prefix_order() {
    let source = "@prefix z: <urn:a:> .\n@prefix long: <urn:a:b:> .\n@prefix m: <urn:a:> .\n\
                  <urn:a:b:s> <urn:a:p> <urn:a:o> .\n";
    assert_eq!(lines(source), vec!["long:s\tm:p\tm:o".to_owned()]);
}

#[test]
fn blank_labels_are_identical_across_parses_and_across_views() {
    let source = b"@prefix atlas: <https://atlas.example/ns#> .\n\
                   [] a atlas:Metric ; atlas:owner [ atlas:name \"revops\" ] .\n\
                   _:authored atlas:items ( 1 2 ) .\n";
    let first = parse_turtle(source, OntologyLimits::default()).expect("parses");
    let second = parse_turtle(source, OntologyLimits::default()).expect("parses");
    assert_eq!(
        first.triples(),
        second.triples(),
        "two parses of one document must render the same text"
    );
    let terms = parse_turtle_terms(source, OntologyLimits::default()).expect("parses");
    assert_eq!(
        terms,
        parse_turtle_terms(source, OntologyLimits::default()).expect("parses")
    );
    let text_subjects: Vec<&str> = first.triples().iter().map(|t| t.subject.as_str()).collect();
    let term_subjects: Vec<&str> = terms.iter().map(|t| t.subject.as_str()).collect();
    assert_eq!(
        text_subjects, term_subjects,
        "a label from one view must name the same node in the other"
    );
    assert!(text_subjects.iter().all(|label| label.starts_with("_:")));
    assert!(
        !text_subjects.contains(&"_:authored"),
        "authored labels are rescoped too: {text_subjects:?}"
    );
}

#[test]
fn both_views_number_a_statement_with_two_new_blank_nodes_alike() {
    // Subject first, then object: a statement whose two ends are both new is
    // where the two views could number differently.
    let source = b"_:x <urn:p> _:y .\n[] <urn:p> [] .\n";
    let text = parse_turtle(source, OntologyLimits::default()).expect("parses");
    let terms = parse_turtle_terms(source, OntologyLimits::default()).expect("parses");
    assert_eq!(terms.len(), 2);
    for (text_row, term_row) in text.triples().iter().zip(&terms) {
        assert_eq!(text_row.subject, term_row.subject);
        assert_eq!(TermView::Blank(text_row.object.clone()), term_row.object);
    }
    let scope = terms[0]
        .subject
        .strip_suffix("b0")
        .expect("the first blank node is numbered 0");
    assert_eq!(terms[0].object, TermView::Blank(format!("{scope}b1")));
    assert_eq!(terms[1].subject, format!("{scope}b2"));
    assert_eq!(terms[1].object, TermView::Blank(format!("{scope}b3")));
}

#[test]
fn the_same_authored_label_in_two_documents_names_two_nodes() {
    // The app adds every module's terms to one graph, so a label must not be
    // shared between documents (RDF 1.2 Concepts §3.5: labels are document-local).
    let one =
        parse_turtle_terms(b"_:r <urn:p> \"one\" .\n", OntologyLimits::default()).expect("parses");
    let two =
        parse_turtle_terms(b"_:r <urn:p> \"two\" .\n", OntologyLimits::default()).expect("parses");
    assert_ne!(one[0].subject, two[0].subject);
}

#[test]
fn one_blank_node_keeps_one_label_in_subject_and_object_position() {
    let terms = parse_turtle_terms(
        b"_:x <urn:p> _:y .\n_:y <urn:p> _:x .\n",
        OntologyLimits::default(),
    )
    .expect("parses");
    assert_eq!(terms.len(), 2);
    assert_eq!(TermView::Blank(terms[1].subject.clone()), terms[0].object);
    assert_eq!(TermView::Blank(terms[0].subject.clone()), terms[1].object);
    assert_ne!(terms[0].subject, terms[1].subject);
}

// --- refusals -------------------------------------------------------------

type View = fn(&[u8], OntologyLimits) -> Result<usize, OntologyError>;

fn text_view(source: &[u8], limits: OntologyLimits) -> Result<usize, OntologyError> {
    parse_turtle(source, limits).map(|ontology| ontology.triples().len())
}

fn term_view(source: &[u8], limits: OntologyLimits) -> Result<usize, OntologyError> {
    parse_turtle_terms(source, limits).map(|triples| triples.len())
}

const VIEWS: [(&str, View); 2] = [("text", text_view), ("terms", term_view)];

#[test]
fn malformed_turtle_is_a_syntax_refusal_in_both_views() {
    for (name, view) in VIEWS {
        for source in [
            &b"<urn:s> <urn:p> .\n"[..],
            b"<urn:s> <urn:p> \"unterminated .\n",
            b"undeclared:s <urn:p> <urn:o> .\n",
            b"<urn:s> <urn:p> <urn:o> .\n@prefix broken: <urn:x",
        ] {
            let error = view(source, OntologyLimits::default()).expect_err("must refuse");
            assert!(
                matches!(error, OntologyError::Syntax(_)),
                "{name}: {error:?}"
            );
            assert_eq!(error.code(), "syntax");
        }
    }
}

#[test]
fn non_utf8_input_is_a_syntax_refusal_in_both_views() {
    for (name, view) in VIEWS {
        // In a term, and in a comment the tokenizer would otherwise skip over.
        for source in [
            &b"<urn:s> <urn:p> \"\xff\" .\n"[..],
            b"# \xff\n<urn:s> <urn:p> <urn:o> .\n",
        ] {
            let error = view(source, OntologyLimits::default()).expect_err("refuses");
            assert!(
                matches!(error, OntologyError::Syntax(_)),
                "{name}: {error:?}"
            );
        }
    }
}

#[test]
fn a_triple_term_is_refused_in_both_views() {
    for (name, view) in VIEWS {
        let error = view(
            b"<urn:s> <urn:p> <<( <urn:a> <urn:b> <urn:c> )>> .\n",
            OntologyLimits::default(),
        )
        .expect_err("a triple term has no text form");
        assert_eq!(
            error,
            OntologyError::TripleTerm { position: "object" },
            "{name}"
        );
        assert_eq!(error.code(), "triple_term");
    }
}

#[test]
fn a_base_direction_is_refused_by_the_term_view_and_erased_by_the_text_view() {
    let source = b"<urn:s> <urn:p> \"x\"@ar--rtl .\n";
    let error = term_view(source, OntologyLimits::default()).expect_err("no direction slot");
    assert_eq!(error, OntologyError::BaseDirection { position: "object" });
    assert_eq!(error.code(), "base_direction");
    // The text contract keeps a literal's lexical form only; direction goes
    // the way of the language tag and the datatype.
    assert_eq!(text_view(source, OntologyLimits::default()), Ok(1));
}

#[test]
fn the_source_byte_budget_is_checked_before_parsing_in_both_views() {
    let source = b"<urn:s> <urn:p> <urn:o> .\n";
    for (name, view) in VIEWS {
        let at_limit = OntologyLimits {
            max_source_bytes: source.len(),
            ..OntologyLimits::default()
        };
        assert_eq!(
            view(source, at_limit),
            Ok(1),
            "{name}: the limit is inclusive"
        );
        let error = view(
            source,
            OntologyLimits {
                max_source_bytes: source.len() - 1,
                ..OntologyLimits::default()
            },
        )
        .expect_err("one byte over");
        assert_eq!(
            error,
            OntologyError::ResourceLimit {
                resource: "source_bytes",
                limit: source.len() - 1,
                observed: source.len(),
            },
            "{name}"
        );
        assert_eq!(error.code(), "resource_limit");
    }
}

#[test]
fn the_statement_budget_counts_distinct_triples_in_both_views() {
    let source = b"<urn:a> <urn:p> 1 .\n<urn:a> <urn:p> 1 .\n<urn:b> <urn:p> 2 .\n";
    for (name, view) in VIEWS {
        let limits = |max_statements| OntologyLimits {
            max_statements,
            ..OntologyLimits::default()
        };
        assert_eq!(
            view(source, limits(2)),
            Ok(2),
            "{name}: duplicates are free"
        );
        assert_eq!(
            view(source, limits(1)).expect_err("second distinct triple"),
            OntologyError::ResourceLimit {
                resource: "statements",
                limit: 1,
                observed: 2,
            },
            "{name}"
        );
    }
}

#[test]
fn every_error_variant_has_a_distinct_stable_code() {
    // `NamedGraph` cannot be produced from Turtle (the grammar has no graph
    // block), so its arm is defensive and is pinned here by construction only.
    let codes = [
        OntologyError::Syntax(String::new()).code(),
        OntologyError::ResourceLimit {
            resource: "statements",
            limit: 0,
            observed: 1,
        }
        .code(),
        OntologyError::TripleTerm { position: "object" }.code(),
        OntologyError::BaseDirection { position: "object" }.code(),
        OntologyError::NamedGraph("urn:g".to_owned()).code(),
    ];
    assert_eq!(
        codes,
        [
            "syntax",
            "resource_limit",
            "triple_term",
            "base_direction",
            "named_graph"
        ]
    );
}

#[test]
fn a_trig_graph_block_is_a_syntax_refusal_not_a_named_graph() {
    let error = term_view(
        b"<urn:g> { <urn:s> <urn:p> <urn:o> . }\n",
        OntologyLimits::default(),
    )
    .expect_err("Turtle has no graph block");
    assert!(matches!(error, OntologyError::Syntax(_)), "{error:?}");
}

// --- index and limits -----------------------------------------------------

#[test]
fn select_equals_a_full_scan_for_every_pattern_shape() {
    let source = "@prefix : <urn:x:> .\n\
                  :a a :Route ; :p :b , :c ; :q \"b\" .\n\
                  :b a :Route ; :p :c .\n\
                  :c a :Metric ; :p :a ; :q \":b\" .\n";
    let ontology =
        parse_turtle(source.as_bytes(), OntologyLimits::default()).expect("document parses");
    let triples = ontology.triples();
    assert_eq!(triples.len(), 9);

    let terms = [
        None,
        Some(":a"),
        Some(":b"),
        Some("a"),
        Some(":p"),
        Some(":Route"),
        Some(":absent"),
    ];
    let mut non_empty = 0;
    for subject in terms {
        for predicate in terms {
            for object in terms {
                let selected = ontology.index().select(subject, predicate, object);
                let scanned: Vec<u32> =
                    if subject.is_none() && predicate.is_none() && object.is_none() {
                        Vec::new()
                    } else {
                        (0u32..)
                            .zip(triples)
                            .filter(|(_, triple)| {
                                subject.is_none_or(|value| triple.subject == value)
                                    && predicate.is_none_or(|value| triple.predicate == value)
                                    && object.is_none_or(|value| triple.object == value)
                            })
                            .map(|(ordinal, _)| ordinal)
                            .collect()
                    };
                assert_eq!(
                    selected, scanned,
                    "({subject:?}, {predicate:?}, {object:?})"
                );
                non_empty += usize::from(!selected.is_empty());
            }
        }
    }
    assert!(
        non_empty >= 15,
        "the fixture must exercise real matches: {non_empty}"
    );
    assert_eq!(
        ontology.index().select(None, Some("a"), Some(":Route")),
        vec![0, 4]
    );
    assert_eq!(
        ontology.index().select(Some(":a"), Some(":p"), Some(":c")),
        vec![2]
    );
    assert!(ontology.index().select(None, None, None).is_empty());
}

#[test]
fn limit_overrides_replace_only_the_ceilings_given() {
    let defaults = OntologyLimits::default();
    let untouched = OntologyLimits::with_overrides(None, None);
    assert_eq!(untouched.max_source_bytes, defaults.max_source_bytes);
    assert_eq!(untouched.max_statements, defaults.max_statements);
    let tightened = OntologyLimits::with_overrides(Some(7), None);
    assert_eq!(tightened.max_source_bytes, 7);
    assert_eq!(tightened.max_statements, defaults.max_statements);
    let both = OntologyLimits::with_overrides(None, Some(0));
    assert_eq!(both.max_source_bytes, defaults.max_source_bytes);
    assert_eq!(both.max_statements, 0);
}

#[test]
fn declared_prefixes_reads_the_parser_table_and_refuses_what_parse_refuses() {
    use ontology_fast_core::declared_prefixes;

    let source =
        b"@base <https://atlas.example/dir/> .\n@prefix rel: <ns#> .\nPREFIX abs: <urn:abs:>\n";
    let table = declared_prefixes(source, OntologyLimits::default()).expect("parses");
    let pairs: Vec<(&str, &str)> = table
        .iter()
        .map(|(prefix, namespace)| (prefix.as_str(), namespace.as_str()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("abs", "urn:abs:"),
            ("rel", "https://atlas.example/dir/ns#")
        ]
    );

    assert!(matches!(
        declared_prefixes(b"@prefix broken: <urn:x", OntologyLimits::default()),
        Err(OntologyError::Syntax(_))
    ));
    assert!(matches!(
        declared_prefixes(source, OntologyLimits::with_overrides(Some(3), None)),
        Err(OntologyError::ResourceLimit {
            resource: "source_bytes",
            ..
        })
    ));
}
