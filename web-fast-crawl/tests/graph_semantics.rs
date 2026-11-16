use web_fast_core::{parse_html, ParseOptions};
use web_fast_crawl::{
    graph::{GraphBuilder, GraphLimits, Term},
    semantic::{extract_semantics, RobotsDirectives, SemanticOptions, LINK, RDF_TYPE},
};
fn statement(builder: &mut GraphBuilder, subject: &str, object: &str, source: &str) {
    builder
        .insert(
            Term::iri(subject),
            Term::iri(LINK),
            Term::iri(object),
            None,
            Term::iri(source),
        )
        .unwrap();
}
#[test]
fn interning_deduplication_and_bidirectional_indexes_preserve_provenance() {
    let mut builder = GraphBuilder::new(GraphLimits::default());
    for _ in 0..5 {
        statement(
            &mut builder,
            "https://e.test/a",
            "https://e.test/b",
            "https://e.test/a",
        );
    }
    statement(
        &mut builder,
        "https://e.test/a",
        "https://e.test/b",
        "https://e.test/c",
    );
    statement(
        &mut builder,
        "https://e.test/b",
        "https://e.test/c",
        "https://e.test/b",
    );
    let graph = builder.finish();
    let a = graph.find(&Term::iri("https://e.test/a")).unwrap();
    let b = graph.find(&Term::iri("https://e.test/b")).unwrap();
    let c = graph.find(&Term::iri("https://e.test/c")).unwrap();
    let predicate = graph.find(&Term::iri(LINK)).unwrap();
    assert_eq!(graph.term_count(), 4);
    assert_eq!(graph.edges().len(), 3);
    assert_eq!(graph.outgoing(a).count(), 2);
    assert_eq!(graph.incoming(b).count(), 2);
    assert_eq!(graph.predicate(predicate).count(), 3);
    assert_eq!(graph.reachable(a, Some(predicate), 5, 10), [a, b, c]);
    assert_eq!(graph.reachable(a, Some(predicate), 1, 10), [a, b]);
    assert_eq!(graph.reachable(a, Some(predicate), 8, 1), [a]);
    assert_eq!(graph.outgoing(u32::MAX).count(), 0);
}
#[test]
fn pagerank_deduplicates_provenance_and_redistributes_dangling_mass() {
    let mut builder = GraphBuilder::new(GraphLimits::default());
    statement(
        &mut builder,
        "https://e.test/a",
        "https://e.test/b",
        "https://e.test/a",
    );
    statement(
        &mut builder,
        "https://e.test/a",
        "https://e.test/b",
        "https://e.test/c",
    );
    let graph = builder.finish();
    let predicate = graph.find(&Term::iri(LINK)).unwrap();
    let ranks = graph.page_rank(predicate, 0.85, 200, 1e-12).unwrap();
    assert!((ranks.iter().map(|(_, score)| score).sum::<f64>() - 1.0).abs() < 1e-9);
    let b = graph.find(&Term::iri("https://e.test/b")).unwrap();
    assert!((ranks.iter().find(|(node, _)| *node == b).unwrap().1 - 0.649122807).abs() < 1e-8);
    assert!(graph.page_rank(predicate, f64::NAN, 10, 0.1).is_err());
    assert!(graph.page_rank(predicate, 0.85, 0, 0.1).is_err());
}
#[test]
fn graph_limits_and_rdf_positions_are_enforced() {
    let mut builder = GraphBuilder::new(GraphLimits {
        max_edges: 1,
        ..GraphLimits::default()
    });
    statement(
        &mut builder,
        "https://e.test/a",
        "https://e.test/b",
        "https://e.test/a",
    );
    assert!(builder
        .insert(
            Term::iri("https://e.test/b"),
            Term::iri(LINK),
            Term::text("new"),
            None,
            Term::iri("https://e.test/a")
        )
        .is_err());
    assert!(builder
        .insert(
            Term::text("bad subject"),
            Term::iri(LINK),
            Term::text("x"),
            None,
            Term::iri("https://e.test/a")
        )
        .is_err());
    let mut builder = GraphBuilder::new(GraphLimits {
        max_term_bytes: 3,
        ..GraphLimits::default()
    });
    assert!(builder
        .insert(
            Term::iri("https://e.test/"),
            Term::iri(LINK),
            Term::text("x"),
            None,
            Term::iri("https://e.test/")
        )
        .is_err());
}
fn semantic(html: &str, source: &str) -> web_fast_crawl::semantic::SemanticDocument {
    extract_semantics(
        &parse_html(html, Some(source), ParseOptions::default()).unwrap(),
        source,
        &SemanticOptions::default(),
    )
    .unwrap()
}
#[test]
fn jsonld_uses_offline_schema_context_and_preserves_typed_literals() {
    let document = semantic(
        r##"<script type="application/ld+json">{"@context":"https://schema.org","@id":"#alice","@type":"Person","name":"Alice","url":{"@id":"/profile"},"age":42}</script>"##,
        "https://e.test/page",
    );
    assert!(
        document.diagnostics.is_empty(),
        "{:?}",
        document.diagnostics
    );
    assert!(document
        .statements
        .iter()
        .any(|s| s.subject == Term::iri("https://e.test/page#alice")
            && s.predicate == Term::iri(RDF_TYPE)
            && s.object == Term::iri("http://schema.org/Person")));
    assert!(document.statements.iter().any(|s| s.object
        == Term::Literal {
            value: "42".into(),
            datatype: "http://www.w3.org/2001/XMLSchema#integer".into(),
            language: None
        }));
    assert!(document
        .link_candidates
        .contains(&"https://e.test/profile".into()));
}
#[test]
fn named_graphs_language_literals_and_blank_scopes_survive() {
    let html = r##"<script type="application/ld+json">{"@context":{"ex":"https://e.test/vocab/"},"@id":"https://e.test/graph","@graph":[{"@id":"_:same","ex:name":{"@value":"Olá","@language":"pt"}}]}</script>"##;
    let first = semantic(html, "https://e.test/one");
    let repeat = semantic(html, "https://e.test/one");
    let second = semantic(html, "https://e.test/two");
    assert_eq!(first.statements, repeat.statements);
    let assertion = first.statements.iter().find(|s| s.graph.is_some()).unwrap();
    assert_eq!(assertion.graph, Some(Term::iri("https://e.test/graph")));
    assert!(
        matches!(&assertion.object, Term::Literal { language: Some(lang), .. } if lang == "pt")
    );
    assert_ne!(
        assertion.subject,
        second
            .statements
            .iter()
            .find(|s| s.graph.is_some())
            .unwrap()
            .subject
    );
}
#[test]
fn unknown_contexts_reject_whole_scripts_without_network_or_payload_diagnostics() {
    let result = semantic(
        r#"<script type="application/ld+json">{"@context":"http://127.0.0.1/private-secret","@type":"Person","name":"private payload"}</script>"#,
        "https://e.test/",
    );
    assert_eq!(result.diagnostics, ["json_ld_rejected:0"]);
    assert!(!serde_json::to_string(&result.statements)
        .unwrap()
        .contains("private payload"));
}
#[test]
fn semantic_limits_fail_explicitly() {
    let document = parse_html(
        "<h1>Title</h1><p>Body</p><script type=application/ld+json>{}</script>",
        Some("https://e.test/"),
        ParseOptions::default(),
    )
    .unwrap();
    assert!(extract_semantics(
        &document,
        "https://e.test/",
        &SemanticOptions {
            max_statements: 1,
            ..SemanticOptions::default()
        }
    )
    .is_err());
    assert!(extract_semantics(
        &document,
        "https://e.test/",
        &SemanticOptions {
            max_jsonld_bytes: 1,
            ..SemanticOptions::default()
        }
    )
    .is_err());
}
#[test]
fn metadata_and_heading_hierarchy_have_provenance() {
    let document = parse_html("<meta property=og:title content='OG title'><meta name=robots content='noindex,nofollow'><h1>One</h1><p>A</p><h2>Two</h2><p>B</p>", Some("https://e.test/"), ParseOptions::default()).unwrap();
    let robots = RobotsDirectives::from_document(&document, "web-fast-crawl");
    assert!(robots.noindex && robots.nofollow);
    let result =
        extract_semantics(&document, "https://e.test/", &SemanticOptions::default()).unwrap();
    let links = result
        .statements
        .iter()
        .filter(|s| s.predicate == Term::iri("https://schema.org/hasPart"))
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 4);
    assert_eq!(links[1].subject, links[0].object);
    assert_eq!(links[2].subject, links[0].object);
    assert_eq!(links[3].subject, links[2].object);
    let mut graph = GraphBuilder::new(GraphLimits::default());
    result.insert_into(&mut graph).unwrap();
    let graph = graph.finish();
    assert!(graph
        .edges()
        .iter()
        .all(|edge| graph.term(edge.source) == Some(&Term::iri("https://e.test/"))));
}

#[test]
fn triple_patterns_match_the_smallest_index_without_losing_sources() {
    let mut builder = GraphBuilder::new(GraphLimits::default());
    for i in 0..100 {
        statement(
            &mut builder,
            &format!("https://e.test/{i}"),
            "https://e.test/shared",
            "https://e.test/source",
        );
    }
    let graph = builder.finish();
    let subject = graph.find(&Term::iri("https://e.test/42")).unwrap();
    let object = graph.find(&Term::iri("https://e.test/shared")).unwrap();
    let predicate = graph.find(&Term::iri(LINK)).unwrap();
    assert_eq!(
        graph
            .matching(Some(subject), Some(predicate), Some(object))
            .count(),
        1
    );
    assert_eq!(graph.matching(None, None, Some(object)).count(), 100);
    assert_eq!(graph.matching(Some(u32::MAX), None, None).count(), 0);
    assert_eq!(graph.matching(None, None, None).count(), 100);
}
#[test]
fn explicit_context_snapshots_and_lists_are_resolved_offline() {
    use std::{collections::BTreeMap, sync::Arc};
    let document = parse_html(r#"<script type=application/ld+json>{"@context":"https://e.test/context","@id":"https://e.test/entity","name":{"@value":"Nome","@language":"pt"},"items":{"@list":["a","b"]}}</script>"#, Some("https://e.test/"), ParseOptions::default()).unwrap();
    let options = SemanticOptions {
        contexts: Arc::new(BTreeMap::from([(
            "https://e.test/context".into(),
            br#"{"@context":{"@vocab":"https://e.test/v/"}}"#.to_vec(),
        )])),
        ..SemanticOptions::default()
    };
    let result = extract_semantics(&document, "https://e.test/", &options).unwrap();
    assert!(result.diagnostics.is_empty());
    assert!(result
        .statements
        .iter()
        .any(|s| s.predicate == Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#first")));
}
#[test]
fn invalid_iris_and_language_datatypes_do_not_enter_the_graph() {
    let mut builder = GraphBuilder::new(GraphLimits::default());
    assert!(builder
        .insert(
            Term::iri("not an iri"),
            Term::iri(LINK),
            Term::text("x"),
            None,
            Term::iri("https://e.test/")
        )
        .is_err());
    assert!(builder
        .insert(
            Term::iri("https://e.test/a"),
            Term::iri(LINK),
            Term::Literal {
                value: "x".into(),
                datatype: "https://e.test/wrong".into(),
                language: Some("en".into())
            },
            None,
            Term::iri("https://e.test/")
        )
        .is_err());
}
