//! Malformed HTML must be parsed within its limits or rejected with an error.
//!
//! Covers `parse_html` and `parse_html_bytes`, with and without a base URL and
//! with odd encoding labels, plus the Arrow output behind the `arrow` feature.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use web_fast_core::{
    parse_html, parse_html_bytes, ParseOptions, WebDocument, WebParseError, MAX_NESTING_DEPTH,
};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

fn seed() -> Vec<u8> {
    br#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Seed &amp; page</title>
<base href="https://example.org/dir/"><link rel="canonical" href="../canon">
<meta name="description" content="A page">
<script type="application/ld+json">{"@context":"https://schema.org","@type":"Thing","n":[1,2]}</script>
<style>p{color:red}</style></head>
<body><h1>Title</h1><p>Para <a href="a/b?c=1#f" rel="nofollow">link</a> <img src="i.png" alt="i" width="1" height="2"></p>
<ul><li>one<li>two</ul><blockquote>quote</blockquote><pre> pre  text </pre>
<dl><dt>term<dd>desc</dl><figure><figcaption>cap</figcaption></figure>
<table><caption>T</caption><thead><tr><th colspan="2">H</th></tr></thead>
<tbody><tr><td rowspan="2">a</td><td>b</td></tr><tr><td>c<table><tr><td>inner</td></tr></table></td></tr></tbody></table>
<div hidden><p>hidden</p></div><p aria-hidden="true">x</p><template><p>t</p></template>
<map><area href="mailto:a@example.org"></map><svg><a href="s">s</a></svg><!-- c --></body></html>
"#
    .to_vec()
}

const LABELS: &[&str] = &[
    "utf-8",
    " UTF-8 ",
    "utf-16le",
    "utf-16be",
    "windows-1252",
    "shift_jis",
    "iso-2022-jp",
    "gb18030",
    "x-user-defined",
    "replacement",
    "no-such-encoding",
    "",
    "\u{0}",
];

fn labelled(label: &str) -> ParseOptions {
    ParseOptions {
        encoding_label: Some(label.to_owned()),
        ..ParseOptions::default()
    }
}

#[cfg(feature = "arrow")]
fn emit(document: &WebDocument) {
    let _ = web_fast_core::arrow::web_blocks_to_record_batch(document);
    let mut sink = Vec::new();
    let _ = web_fast_core::arrow::write_web_blocks_ipc(document, &mut sink);
}

#[cfg(not(feature = "arrow"))]
fn emit(_document: &WebDocument) {}

/// Parses `bytes` through every entry point; one label per input keeps the
/// run short while the whole label list is covered across inputs.
fn exercise(bytes: &[u8]) {
    if let Ok(document) = parse_html_bytes(bytes, None, ParseOptions::default()) {
        emit(&document);
    }
    let label = LABELS[bytes.len() % LABELS.len()];
    let base = [
        "https://example.org/a/b?c#d",
        "not a url",
        "data:text/plain,x",
        "",
    ][bytes.len() % 4];
    if let Ok(document) = parse_html_bytes(bytes, Some(base), labelled(label)) {
        emit(&document);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let _ = parse_html(text, Some(base), ParseOptions::default());
    }
}

#[test]
fn mutated_html_never_panics() {
    // 20,000 iterations ran clean once but take over a minute in a debug
    // build; this count keeps the test near 15 seconds.
    smoke::assert_no_panics("web-fast-core html", &[seed()], 4_000, exercise);
}

#[test]
fn every_encoding_label_is_handled() {
    let mut inputs = vec![seed(), b"\xff\xfe<\x00p\x00>\x00".to_vec(), vec![0xff; 64]];
    inputs.push(b"\xef\xbb\xbf<meta charset=\"".to_vec());
    inputs.push(b"<meta charset=utf-16le><p>x</p>".to_vec());
    for input in &inputs {
        for label in LABELS {
            let _ = parse_html_bytes(input, None, labelled(label));
            let _ = parse_html_bytes(input, Some("https://example.org/"), labelled(label));
        }
    }
}

#[test]
fn limits_are_errors_not_truncation() {
    let tiny = ParseOptions {
        max_input_bytes: 16,
        ..ParseOptions::default()
    };
    assert!(parse_html_bytes(&seed(), None, tiny).is_err());
    for (max_nodes, max_text_bytes) in [(0, usize::MAX), (usize::MAX, 0), (1, 1)] {
        let options = ParseOptions {
            max_nodes,
            max_text_bytes,
            ..ParseOptions::default()
        };
        assert!(parse_html_bytes(&seed(), None, options).is_err());
    }
}

#[test]
fn huge_spans_do_not_expand_tables() {
    let mut html = String::from("<table>");
    for span in [
        "4294967295",
        "4294967296",
        "99999999999999999999",
        "-1",
        "1e9",
    ] {
        html.push_str(&format!(
            "<tr><td colspan={span} rowspan={span}>a</td><th colspan=\"{span}\">b</th></tr>"
        ));
    }
    html.push_str("</table>");
    let document = parse_html(&html, None, ParseOptions::default()).expect("parses");
    assert_eq!(document.tables.len(), 1);
    assert_eq!(document.tables[0].rows.len(), 5);
    assert!(document.tables[0].rows.iter().all(|row| row.len() == 2));
    emit(&document);
}

#[test]
fn deeply_nested_divs_are_bounded_on_a_small_stack() {
    for (open, close) in [
        (&b"<div>"[..], &b"</div>"[..]),
        (b"<div>", b""),
        (b"<span><b><i>", b""),
    ] {
        let document = smoke::nested(open, b"<p>leaf</p>", close, 2_000);
        smoke::run_on_small_stack(move || exercise(&document));
    }
}

/// 100,000 nested `<div>` did not finish within 60 seconds in a debug build.
///
/// The time went to the HTML tree builder, which walks its stack of open
/// elements for every block-level start tag (5,000 / 10,000 / 20,000 levels
/// took 1.1 s / 5.4 s / 24 s on their own). Input nested deeper than
/// `MAX_NESTING_DEPTH` is now refused before it reaches the tree builder.
#[test]
fn a_hundred_thousand_nested_divs_are_bounded_on_a_small_stack() {
    let document = smoke::nested(b"<div>", b"<p>leaf</p>", b"</div>", 100_000);
    smoke::run_on_small_stack(move || exercise(&document));
}

fn parse_text(html: &str) -> Result<WebDocument, WebParseError> {
    parse_html(html, None, ParseOptions::default())
}

#[test]
fn nesting_beyond_the_limit_is_an_error_not_truncation() {
    // `html` and `body` are the first two levels.
    let at_limit = "<div>".repeat(MAX_NESTING_DEPTH - 2) + "x";
    let document = parse_text(&at_limit).expect("nesting at the limit parses");
    assert_eq!(document.text, "x");
    for html in [
        "<div>".repeat(MAX_NESTING_DEPTH - 1) + "x",
        "<div>".repeat(100_000) + "<p>leaf</p>",
        "<blockquote>".repeat(100_000) + "x",
        "<ul><li>".repeat(MAX_NESTING_DEPTH) + "x",
        "<table><tr><td>".repeat(MAX_NESTING_DEPTH) + "x",
    ] {
        assert!(matches!(
            parse_text(&html),
            Err(WebParseError::NestingLimitExceeded)
        ));
    }
}

/// Long documents that are not deeply nested must not trip the nesting limit:
/// implied end tags, void elements, self-closing SVG shapes, and markup that
/// only appears inside a script, a comment or an attribute value.
#[test]
fn long_flat_documents_are_not_mistaken_for_deep_ones() {
    let fake = "<div>".repeat(5_000);
    let cases = [
        ("<p>x".repeat(3_000), 3_000),
        (format!("<ul>{}</ul>", "<li>x".repeat(3_000)), 3_000),
        ("x<br>".repeat(3_000) + "<p>end", 1),
        (
            format!("<table>{}</table>", "<tr><td>a<td>b".repeat(5_000)),
            10_000,
        ),
        (format!("<dl>{}</dl>", "<dt>t<dd>d".repeat(1_500)), 3_000),
        ("<p><font size=2>x<p><b><i>y".repeat(2_000), 4_000),
        (
            format!("<svg>{}</svg><p>end", "<path d=\"M0 0\"/>".repeat(5_000)),
            1,
        ),
        (format!("<script>{fake}</script><p>end"), 1),
        (format!("<!-- {fake} --><p title=\"{fake}\">end"), 1),
        (
            format!("<select>{}</select><p>end", "<option>o".repeat(5_000)),
            1,
        ),
    ];
    for (html, blocks) in cases {
        let document = parse_text(&html).unwrap_or_else(|error| {
            panic!("{error}: {}", &html[..html.len().min(60)]);
        });
        assert_eq!(document.blocks.len(), blocks, "{}", &html[..40]);
    }
}

#[test]
fn nested_tables_and_blocks_are_bounded_on_a_small_stack() {
    for (open, close, depth) in [
        (&b"<table><tr><td>"[..], &b"</td></tr></table>"[..], 30),
        (b"<blockquote>", b"</blockquote>", 200),
        (b"<ul><li>", b"</li></ul>", 200),
    ] {
        let document = smoke::nested(open, b"x", close, depth);
        smoke::run_on_small_stack(move || exercise(&document));
    }
}

/// Each table owns its own rows and each row its own cells.
///
/// Before the fix every table repeated the rows of every table nested in it
/// and every row repeated the cells of every nested row, so 100 levels
/// produced 5,050 rows and 171,700 cells and visited 17 million nodes for
/// their text. The shape below is linear in the nesting depth.
#[test]
fn nested_tables_report_only_their_own_rows_and_cells() {
    let depth = 100;
    let html = smoke::nested(b"<table><tr><td>", b"x", b"</td></tr></table>", depth);
    let html = String::from_utf8(html).expect("ascii");
    let document = parse_html(&html, None, ParseOptions::default()).expect("parses");
    assert_eq!(document.tables.len(), depth);
    for table in &document.tables {
        assert_eq!(table.rows, [["x"]], "table {}", table.ordinal);
    }
}

/// 300 nested tables ran for more than 60 seconds in a debug build because
/// every table repeated the rows and cells of the tables inside it; 100,000
/// nested block quotes were never run. Both now exceed `MAX_NESTING_DEPTH`
/// and are refused; `nested_tables_report_only_their_own_rows_and_cells`
/// covers the table shape inside the limit.
#[test]
fn deeply_nested_tables_and_text_blocks_are_bounded() {
    let document = smoke::nested(b"<table><tr><td>", b"x", b"</td></tr></table>", 300);
    smoke::run_on_small_stack(move || exercise(&document));
    let document = smoke::nested(b"<blockquote>", b"x", b"</blockquote>", 100_000);
    smoke::run_on_small_stack(move || exercise(&document));
}
