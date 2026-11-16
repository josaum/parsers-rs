use web_fast_core::{parse_html, parse_html_bytes, ParseOptions, WebParseError};

fn parse(html: &str) -> web_fast_core::WebDocument {
    parse_html(
        html,
        Some("https://example.test/path/page"),
        ParseOptions::default(),
    )
    .unwrap()
}

#[test]
fn visible_text_preserves_inline_boundaries_and_excludes_hidden_descendants() {
    let doc = parse("<p>hel<b>lo</b>, <em>world</em>!<span hidden>SECRET</span><script>secret()</script><br>next&nbsp;word</p>");
    assert_eq!(doc.blocks[0].text, "hello, world! next\u{a0}word");
    assert_eq!(doc.text, "hello, world!\nnext\u{a0}word");
}

#[test]
fn document_text_keeps_unwrapped_content_without_duplicate_nested_blocks() {
    let doc = parse("<body>Intro<div>Outside</div><blockquote><p>Quoted</p></blockquote><ul><li>Parent<ul><li>Child</li></ul></li></ul>Tail</body>");
    assert_eq!(doc.text, "Intro\nOutside\nQuoted\nParent\nChild\nTail");
    assert_eq!(doc.text.matches("Quoted").count(), 1);
}

#[test]
fn preformatted_text_preserves_indentation_and_line_breaks() {
    let doc = parse("<pre>  a\n    b\t&lt;x&gt;</pre>");
    assert_eq!(doc.blocks[0].text, "  a\n    b\t<x>");
    assert_eq!(doc.text, "  a\n    b\t<x>");
}

#[test]
fn tables_keep_empty_cells_and_isolate_nested_tables() {
    let doc = parse("<table><caption>Outer</caption><tr><td>A</td><td></td><td>C<table><caption>Inner</caption><tr><td>X</td></tr></table></td></tr></table>");
    assert_eq!(doc.tables[0].rows, vec![vec!["A", "", "C"]]);
    assert_eq!(doc.tables[1].rows, vec![vec!["X"]]);
    assert_eq!(doc.tables[0].caption.as_deref(), Some("Outer"));
    assert_eq!(doc.tables[1].caption.as_deref(), Some("Inner"));
}

#[test]
fn metadata_is_case_insensitive_and_unsafe_base_falls_back() {
    let doc = parse("<base href='javascript:bad'><base href='https://other.test/'><meta name='DESCRIPTION' content='Summary'><link rel='alternate CANONICAL' href=''><a href=''>same</a><a href='#section'>jump</a><script type='APPLICATION/LD+JSON'>{\"ok\":true}</script>");
    assert_eq!(doc.description.as_deref(), Some("Summary"));
    assert_eq!(
        doc.base_url.as_deref(),
        Some("https://example.test/path/page")
    );
    assert_eq!(doc.canonical_url, doc.base_url);
    assert_eq!(doc.links[0].resolved_url, doc.base_url);
    assert_eq!(doc.json_ld[0]["ok"], true);
}

#[test]
fn hidden_resources_are_excluded_but_visible_occurrences_remain() {
    let doc = parse("<div hidden><a href='/hidden'>hidden</a><img src='hidden.png'><table><tr><td>hidden</td></tr></table></div><a href='/ok'>ok</a><img src='ok.png'>");
    assert_eq!(doc.links.len(), 1);
    assert_eq!(doc.images.len(), 1);
    assert!(doc.tables.is_empty());
}

#[test]
fn charset_detection_ignores_comments_and_unrelated_attributes() {
    for prefix in [
        "<!-- <meta charset=windows-1252> -->",
        "<meta data-charset=windows-1252>",
        "<meta content='text/html;charset=windows-1252'>",
        "<meta charset=not-an-encoding>",
    ] {
        let input = format!("{prefix}<meta charset=utf-8><p>café</p>");
        assert_eq!(
            parse_html_bytes(input.as_bytes(), None, ParseOptions::default())
                .unwrap()
                .text,
            "café",
            "{prefix}"
        );
    }
    let doc = parse_html_bytes(
        b"<meta content='text/html; charset=windows-1252' HTTP-EQUIV='Content-Type'><p>caf\xe9</p>",
        None,
        ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(doc.text, "café");
}

#[test]
fn meta_utf16_uses_utf8_and_explicit_opposite_bom_is_rejected() {
    assert_eq!(
        parse_html_bytes(
            b"<meta charset=utf-16><p>hello</p>",
            None,
            ParseOptions::default()
        )
        .unwrap()
        .text,
        "hello"
    );
    let opts = ParseOptions {
        encoding_label: Some("utf-16be".into()),
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html_bytes(&[0xff, 0xfe, b'A', 0], None, opts),
        Err(WebParseError::InvalidEncoding)
    ));
}

#[test]
fn metadata_and_attributes_cannot_bypass_text_limit() {
    for input in [
        "<meta name=description content=abcdefgh>",
        "<img src=abcdefgh>",
        "<html lang=abcdefgh>",
        "<a href=abcdefgh></a>",
    ] {
        let opts = ParseOptions {
            max_text_bytes: 4,
            ..ParseOptions::default()
        };
        assert!(
            matches!(
                parse_html(input, None, opts),
                Err(WebParseError::TextLimitExceeded)
            ),
            "{input}"
        );
    }
}

#[test]
fn paths_are_unique_and_follow_element_sibling_positions() {
    let doc = parse("<div><p>A</p>text<span></span><p>B</p></div><div><p>C</p></div>");
    assert_eq!(doc.blocks[0].dom_path, "/html[1]/body[1]/div[1]/p[1]");
    assert_eq!(doc.blocks[1].dom_path, "/html[1]/body[1]/div[1]/p[2]");
    assert_eq!(doc.blocks[2].dom_path, "/html[1]/body[1]/div[2]/p[1]");
}

#[test]
fn structural_limits_include_unemitted_and_hidden_nodes() {
    let opts = ParseOptions {
        max_dom_nodes: 10,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html(
            &format!("<div hidden>{}</div>", "<span></span>".repeat(20)),
            None,
            opts
        ),
        Err(WebParseError::NodeLimitExceeded)
    ));
    let opts = ParseOptions {
        max_dom_depth: 12,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html(
            &format!("{}text{}", "<div>".repeat(20), "</div>".repeat(20)),
            None,
            opts
        ),
        Err(WebParseError::DepthLimitExceeded)
    ));
    let opts = ParseOptions {
        max_nodes: 2,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html("<table><tr><td></td></tr></table>", None, opts),
        Err(WebParseError::NodeLimitExceeded)
    ));
}

#[test]
fn output_budget_includes_document_text_copies_and_structural_paths() {
    let full = parse_html("<p>A</p>", None, ParseOptions::default()).unwrap();
    let bytes = full.blocks[0].text.len() + full.blocks[0].dom_path.len() + full.text.len();
    for (limit, succeeds) in [(bytes, true), (bytes - 1, false)] {
        let result = parse_html(
            "<p>A</p>",
            None,
            ParseOptions {
                max_text_bytes: limit,
                ..ParseOptions::default()
            },
        );
        assert_eq!(result.is_ok(), succeeds);
    }
}

#[test]
fn encoding_handles_boms_legacy_remapping_and_the_prefix_boundary() {
    let html = "<p>café😀</p>";
    let mut utf16 = vec![0xff, 0xfe];
    utf16.extend(html.encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(
        parse_html_bytes(&utf16, None, ParseOptions::default())
            .unwrap()
            .text,
        "café😀"
    );
    let bytes = b"<meta charset=x-user-defined><p>\x80</p>";
    assert_eq!(
        parse_html_bytes(bytes, None, ParseOptions::default())
            .unwrap()
            .text,
        "€"
    );
    let mut late = vec![b' '; 1024];
    late.extend_from_slice(b"<meta charset=windows-1252><p>\xe9</p>");
    assert!(matches!(
        parse_html_bytes(&late, None, ParseOptions::default()),
        Err(WebParseError::InvalidEncoding)
    ));
    let bytes = b"<script>const fake = '<meta charset=windows-1252>';</script><meta charset=utf-8><p>caf\xc3\xa9</p>";
    assert_eq!(
        parse_html_bytes(bytes, None, ParseOptions::default())
            .unwrap()
            .text,
        "café"
    );
}

#[test]
fn template_content_is_inert_and_malformed_markup_is_recovered() {
    let doc = parse("<template><base href='https://wrong.test/'><meta name=description content=wrong><p>secret</p><a href=/hidden>secret</a><script type=application/ld+json>{\"wrong\":true}</script></template><p>one<p>two");
    assert_eq!(
        doc.base_url.as_deref(),
        Some("https://example.test/path/page")
    );
    assert_eq!(doc.text, "one\ntwo");
    assert_eq!(doc.blocks.len(), 2);
    assert!(doc.description.is_none());
    assert!(doc.links.is_empty());
    assert!(doc.json_ld.is_empty());
}

#[test]
fn file_limit_and_empty_input_are_consistent() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "<p>too large</p>").unwrap();
    let opts = ParseOptions {
        max_input_bytes: 4,
        ..ParseOptions::default()
    };
    assert!(matches!(
        web_fast_core::parse_html_file(file.path(), None, opts),
        Err(WebParseError::InputTooLarge)
    ));
    assert!(parse_html("", None, ParseOptions::default())
        .unwrap()
        .text
        .is_empty());
}

#[test]
fn preformatted_breaks_and_empty_links_have_defined_behavior() {
    let doc = parse("<pre>a<br>b</pre><a href=''>same</a><a href='?q=1'>query</a><a href='//cdn.test/a'>cdn</a>");
    assert_eq!(doc.blocks[0].text, "a\nb");
    assert_eq!(
        doc.links[0].resolved_url.as_deref(),
        Some("https://example.test/path/page")
    );
    assert_eq!(
        doc.links[1].resolved_url.as_deref(),
        Some("https://example.test/path/page?q=1")
    );
    assert_eq!(
        doc.links[2].resolved_url.as_deref(),
        Some("https://cdn.test/a")
    );
}

#[test]
fn semantic_metadata_preserves_repeated_properties_and_jsonld_media_parameters() {
    let doc = parse("<meta property='og:title' content='First'><meta property='og:title' name='twitter:title' content='Second'><template><meta property='og:title' content='Hidden'></template><script type=' APPLICATION/LD+JSON; profile=test '>{\"@id\":\"https://e.test/entity\"}</script>");
    assert_eq!(doc.metadata.len(), 3);
    assert_eq!(doc.metadata[0].name, "og:title");
    assert_eq!(doc.metadata[1].name, "twitter:title");
    assert_eq!(doc.metadata[2].content, "Second");
    assert_eq!(doc.json_ld.len(), 1);
}

#[test]
fn application_json_and_state_assignments_are_extracted_without_evaluating_js() {
    let doc = parse(
        r#"<script id='__NEXT_DATA__' type='application/json'>{"props":{"products":[{"name":"Shoe","price":"12.30"}]}}</script><script>window.__INITIAL_STATE__ = {"cart":[]}; doNotExecute();</script><script>window.__PRELOADED_STATE__ = {"wrong":true} || compute();</script><script type=application/json>{invalid}</script><template><script type=application/json>{"hidden":true}</script></template><link rel=next href='?page=2'><link rel=alternate type=application/json href='/api/products'>"#,
    );
    assert_eq!(doc.json_data.len(), 2);
    assert_eq!(doc.json_data[0].kind, "next_data");
    assert_eq!(
        doc.json_data[0]
            .value
            .pointer("/props/products/0/price")
            .unwrap(),
        "12.30"
    );
    assert_eq!(doc.json_data[1].kind, "initial_state");
    assert_eq!(doc.discovery_links.len(), 2);
    assert!(doc.diagnostics.contains(&"invalid_embedded_json:2".into()));
}
