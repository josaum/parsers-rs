//! A PDF saved with incremental updates is the document its newest trailer
//! describes, not the first one found in the file.
//!
//! Each save appends changed objects, a cross-reference section and a
//! trailer whose `/Prev` points at the section before it. Reading the first
//! `/Catalog` in the file gives the oldest revision: later pages vanish.

use pdf_fast_core::{ParseOptions, PdfDocument};

const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

fn stream(body: &str) -> String {
    format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len())
}

fn page(parent: u32, contents: u32) -> String {
    format!(
        "<< /Type /Page /Parent {parent} 0 R /MediaBox [0 0 200 200] /Contents {contents} 0 R \
         /Resources << /Font << /F1 9 0 R >> >> >>"
    )
}

fn text(word: &str) -> String {
    stream(&format!(
        "0 0 20 20 re f BT /F1 12 Tf 30 100 Td ({word}) Tj ET"
    ))
}

/// Appends `objects` and returns their offsets.
fn write_objects(out: &mut Vec<u8>, objects: &[(u32, String)]) -> Vec<(u32, usize)> {
    let mut offsets = Vec::new();
    for (number, body) in objects {
        offsets.push((*number, out.len()));
        out.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    offsets
}

/// Appends one revision with a classic cross-reference table and returns
/// where that table starts. `startxref` overrides the offset written after
/// the trailer.
fn revision(
    out: &mut Vec<u8>,
    objects: &[(u32, String)],
    root: u32,
    prev: Option<usize>,
    startxref: Option<usize>,
) -> usize {
    let offsets = write_objects(out, objects);
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (number, offset) in offsets {
        out.extend_from_slice(format!("{number} 1\n{offset:010} 00000 n \n").as_bytes());
    }
    let prev = prev.map_or(String::new(), |prev| format!(" /Prev {prev}"));
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size 100 /Root {root} 0 R{prev} >>\nstartxref\n{}\n%%EOF\n",
            startxref.unwrap_or(xref)
        )
        .as_bytes(),
    );
    xref
}

/// Appends one revision whose cross-reference section is a stream (object
/// `number`) and returns where it starts.
fn stream_revision(
    out: &mut Vec<u8>,
    objects: &[(u32, String)],
    number: u32,
    root: u32,
    prev: usize,
) -> usize {
    let mut offsets = write_objects(out, objects);
    let xref = out.len();
    offsets.push((number, xref));
    let mut index = String::new();
    let mut rows = Vec::new();
    for (object, offset) in offsets {
        index.push_str(&format!("{object} 1 "));
        rows.push(1u8);
        rows.extend_from_slice(&u32::try_from(offset).expect("small file").to_be_bytes());
        rows.push(0);
    }
    out.extend_from_slice(
        format!(
            "{number} 0 obj\n<< /Type /XRef /Size 100 /W [1 4 1] /Index [{index}] /Root {root} 0 R \
             /Prev {prev} /Length {} >>\nstream\n",
            rows.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&rows);
    out.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    xref
}

fn first_revision(out: &mut Vec<u8>) -> usize {
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
    revision(
        out,
        &[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
            (3, page(2, 4)),
            (4, text("One")),
            (9, FONT.to_string()),
        ],
        1,
        None,
        None,
    )
}

/// The second revision's objects: a new catalog and page tree with a second
/// page.
fn second_page() -> Vec<(u32, String)> {
    vec![
        (10, "<< /Type /Catalog /Pages 11 0 R >>".to_string()),
        (
            11,
            "<< /Type /Pages /Kids [3 0 R 12 0 R] /Count 2 >>".to_string(),
        ),
        (12, page(11, 13)),
        (13, text("Two")),
    ]
}

fn third_page() -> Vec<(u32, String)> {
    vec![
        (20, "<< /Type /Catalog /Pages 21 0 R >>".to_string()),
        (
            21,
            "<< /Type /Pages /Kids [3 0 R 12 0 R 22 0 R] /Count 3 >>".to_string(),
        ),
        (22, page(21, 23)),
        (23, text("Three")),
    ]
}

/// Three saves, each adding a page under a new catalog.
fn three_revisions(broken_startxref: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let first = first_revision(&mut out);
    let second = revision(&mut out, &second_page(), 10, Some(first), None);
    revision(
        &mut out,
        &third_page(),
        20,
        Some(second),
        broken_startxref.then_some(9_999_999),
    );
    out
}

fn open(bytes: &[u8]) -> PdfDocument {
    PdfDocument::from_slice(bytes, ParseOptions::default()).expect("parses")
}

fn words(document: &PdfDocument) -> String {
    document
        .semantic_text_runs()
        .iter()
        .map(|run| run.text.clone())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn three_revisions_are_three_pages_with_all_their_text() {
    let document = open(&three_revisions(false));
    assert_eq!(document.pages().len(), 3);
    let words = words(&document);
    for word in ["One", "Two", "Three"] {
        assert!(words.contains(word), "{word} missing from {words:?}");
    }
}

#[cfg(feature = "rasterize")]
#[test]
fn every_page_of_three_revisions_renders_through_both_entry_points() {
    let document = open(&three_revisions(false));
    for page in 0..3 {
        let fast = document
            .page_rasterized_png(page, 72.0)
            .expect("fast entry");
        assert!(fast.is_some(), "page {page} did not rasterize");
        let interpreted = document
            .page_rasterized_png_interpreted_checked(page, 72.0)
            .expect("interpreted entry");
        assert!(interpreted.is_some(), "page {page} was not interpreted");
    }
    // Past the end, both entry points say so with the same error.
    let fast = document
        .page_rasterized_png(3, 72.0)
        .expect_err("fast entry");
    let interpreted = document
        .page_rasterized_png_interpreted_checked(3, 72.0)
        .expect_err("interpreted entry");
    assert_eq!(fast.to_string(), "page index 3 out of bounds");
    assert_eq!(interpreted.to_string(), fast.to_string());
    assert!(document.page_rasterized_png_interpreted(3, 72.0).is_none());
}

#[test]
fn a_revision_that_replaces_content_shows_the_new_content() {
    let mut out = Vec::new();
    let first = first_revision(&mut out);
    revision(&mut out, &[(4, text("Replaced"))], 1, Some(first), None);
    let document = open(&out);
    assert_eq!(document.pages().len(), 1);
    let words = words(&document);
    assert!(
        words.contains("Replaced"),
        "new text missing from {words:?}"
    );
    assert!(!words.contains("One"), "superseded text kept in {words:?}");
}

#[test]
fn a_revision_that_drops_a_page_reports_the_smaller_count() {
    let mut out = Vec::new();
    let first = first_revision(&mut out);
    let second = revision(&mut out, &second_page(), 10, Some(first), None);
    revision(
        &mut out,
        &[(11, "<< /Type /Pages /Kids [12 0 R] /Count 1 >>".to_string())],
        10,
        Some(second),
        None,
    );
    let document = open(&out);
    assert_eq!(document.pages().len(), 1);
    let words = words(&document);
    assert!(
        words.contains("Two"),
        "kept page's text missing from {words:?}"
    );
    assert!(
        !words.contains("One"),
        "dropped page's text kept in {words:?}"
    );
}

#[test]
fn a_cross_reference_stream_revision_on_a_classic_one_is_followed() {
    let mut out = Vec::new();
    let first = first_revision(&mut out);
    stream_revision(&mut out, &second_page(), 30, 10, first);
    let document = open(&out);
    assert_eq!(document.pages().len(), 2);
    assert!(words(&document).contains("Two"));
}

#[test]
fn an_older_catalog_written_later_in_the_file_is_not_the_document() {
    // The trailer names catalog 10, with two pages. Catalog 1, with one
    // page, sits after it in the file; only the trailer says which is live.
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut objects = second_page();
    objects.extend([
        (3, page(11, 4)),
        (4, text("One")),
        (9, FONT.to_string()),
        (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
        (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
    ]);
    revision(&mut out, &objects, 10, None, None);
    assert_eq!(open(&out).pages().len(), 2);
}

#[test]
fn a_prev_chain_that_loops_terminates() {
    let mut out = Vec::new();
    first_revision(&mut out);
    // The second revision's `/Prev` is its own table.
    let own_table = out.len()
        + second_page()
            .iter()
            .map(|(number, body)| format!("{number} 0 obj\n{body}\nendobj\n").len())
            .sum::<usize>();
    let table = revision(&mut out, &second_page(), 10, Some(own_table), None);
    assert_eq!(table, own_table);
    assert_eq!(open(&out).pages().len(), 2);
}

#[test]
fn a_broken_startxref_falls_back_to_the_newest_catalog_in_the_file() {
    let document = open(&three_revisions(true));
    assert_eq!(document.pages().len(), 3);
    assert!(words(&document).contains("Three"));
}
