use web_fast_core::{parse_html, parse_html_bytes, BlockKind, ParseOptions, WebParseError};

const HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <title>  Example   Web  </title>
  <base href="https://example.test/docs/">
  <link rel="canonical" href="guide">
  <meta name="description" content="A deterministic page">
  <script type="application/ld+json">{"@type":"TechArticle","name":"Guide"}</script>
  <script>ignored()</script>
</head>
<body>
  <main>
    <h1>Web <em>Guide</em></h1>
    <p>Parse &amp; preserve evidence.</p>
    <p hidden>Secret text</p>
    <ul><li>First</li><li>Second</li></ul>
    <a href="chapter-1"> Read chapter </a>
    <img src="images/diagram.png" alt="Architecture">
    <table><caption>Signals</caption><tr><th>Name</th><th>Value</th></tr><tr><td>A</td><td>1</td></tr></table>
  </main>
</body>
</html>"#;

#[test]
fn parses_deterministically_with_structural_evidence() {
    let first = parse_html(
        HTML,
        Some("https://ignored.test/root"),
        ParseOptions::default(),
    )
    .expect("parse HTML");
    let second = parse_html(
        HTML,
        Some("https://ignored.test/root"),
        ParseOptions::default(),
    )
    .expect("repeat parse");

    assert_eq!(first.source_sha256, second.source_sha256);
    assert_eq!(first.blocks, second.blocks);
    assert_eq!(first.links, second.links);
    assert_eq!(first.images, second.images);
    assert_eq!(first.tables, second.tables);
    assert_eq!(first.title.as_deref(), Some("Example Web"));
    assert_eq!(first.language.as_deref(), Some("en"));
    assert_eq!(
        first.canonical_url.as_deref(),
        Some("https://example.test/docs/guide")
    );
    assert_eq!(
        first.links[0].resolved_url.as_deref(),
        Some("https://example.test/docs/chapter-1")
    );
    assert_eq!(
        first.images[0].resolved_url.as_deref(),
        Some("https://example.test/docs/images/diagram.png")
    );
    assert!(first.blocks.iter().any(|block| {
        block.kind == BlockKind::Heading
            && block.heading_level == Some(1)
            && block.text == "Web Guide"
    }));
    assert!(!first.text.contains("Secret text"));
    assert!(!first.text.contains("ignored"));
    assert!(first
        .blocks
        .iter()
        .all(|block| block.dom_path.starts_with('/')));
    assert_eq!(first.tables[0].caption.as_deref(), Some("Signals"));
    assert_eq!(first.tables[0].rows[1], ["A", "1"]);
    assert_eq!(first.json_ld[0]["@type"], "TechArticle");
}

#[test]
fn path_and_bytes_have_identical_output() {
    let temp = tempfile::NamedTempFile::with_suffix(".html").expect("temp HTML");
    std::fs::write(temp.path(), HTML).expect("write HTML");
    let from_path = web_fast_core::parse_html_file(
        temp.path(),
        Some("https://ignored.test/root"),
        ParseOptions::default(),
    )
    .expect("path parse");
    let from_bytes = parse_html_bytes(
        HTML.as_bytes(),
        Some("https://ignored.test/root"),
        ParseOptions::default(),
    )
    .expect("bytes parse");

    assert_eq!(
        serde_json::to_value(from_path).expect("path JSON"),
        serde_json::to_value(from_bytes).expect("bytes JSON")
    );
}

#[test]
fn rejects_oversized_or_invalidly_encoded_input() {
    let too_small = ParseOptions {
        max_input_bytes: 8,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html(HTML, None, too_small),
        Err(WebParseError::InputTooLarge)
    ));

    assert!(matches!(
        parse_html_bytes(&[0xff], None, ParseOptions::default()),
        Err(WebParseError::InvalidEncoding)
    ));
}

#[test]
fn detects_declared_legacy_encoding_without_lossy_utf8_fallback() {
    let bytes =
        b"<html><head><meta charset='windows-1252'></head><body><p>Caf\xe9</p></body></html>";
    let document =
        parse_html_bytes(bytes, None, ParseOptions::default()).expect("decode declared encoding");
    assert_eq!(document.text, "Café");
}

#[test]
fn rejects_node_and_text_budget_overflow() {
    let node_limited = ParseOptions {
        max_nodes: 1,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html(HTML, None, node_limited),
        Err(WebParseError::NodeLimitExceeded)
    ));

    let text_limited = ParseOptions {
        max_text_bytes: 4,
        ..ParseOptions::default()
    };
    assert!(matches!(
        parse_html(HTML, None, text_limited),
        Err(WebParseError::TextLimitExceeded)
    ));
}

#[test]
fn fallback_text_excludes_non_content_elements() {
    let document = parse_html(
        "<html><body><div>Visible <span>text</span></div><script>secret()</script><div aria-hidden='true'>hidden</div></body></html>",
        None,
        ParseOptions::default(),
    )
    .expect("fallback parse");

    assert_eq!(document.text, "Visible text");
}

#[test]
fn records_invalid_json_ld_without_copying_payload_into_diagnostics() {
    let document = parse_html(
        "<html><body><p>ok</p><script type='application/ld+json'>{private</script></body></html>",
        None,
        ParseOptions::default(),
    )
    .expect("tolerant HTML parse");

    assert_eq!(document.diagnostics, ["invalid_json_ld:1"]);
    assert!(document.json_ld.is_empty());
}

/// `source_sha256` is the SHA-256 of the input bytes as 64 lower-case hex
/// digits; the known answer for `abc` pins the text across `sha2` releases.
#[test]
fn source_sha256_is_the_lowercase_hex_digest_of_the_input() {
    let document = parse_html_bytes(b"abc", None, ParseOptions::default()).expect("parse");
    assert_eq!(
        document.source_sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
