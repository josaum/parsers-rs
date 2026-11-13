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
    // Past the end: an error from the fast entry, nothing from the
    // interpreted one, which is what the language bindings pass on.
    let fast = document
        .page_rasterized_png(3, 72.0)
        .expect_err("fast entry");
    assert_eq!(fast.to_string(), "page index 3 out of bounds");
    let interpreted = document
        .page_rasterized_png_interpreted_checked(3, 72.0)
        .expect("interpreted entry");
    assert!(interpreted.is_none());
    assert!(document.page_rasterized_png_interpreted(3, 72.0).is_none());
}

/// Passes with any rule that prefers a later copy of an object, including
/// plain scan order. It guards that preference itself: the chain must not
/// make a superseded content stream current again.
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

/// Passes under "take the last catalog" too. What it guards is that a
/// `/Prev` pointing at its own section ends the walk and leaves a usable
/// document.
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

/// The fallback itself: with no chain to follow, the catalog latest in the
/// file is the document. "Take the last catalog" is the rule under test.
#[test]
fn a_broken_startxref_falls_back_to_the_newest_catalog_in_the_file() {
    let document = open(&three_revisions(true));
    assert_eq!(document.pages().len(), 3);
    assert!(words(&document).contains("Three"));
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("compress");
    encoder.finish().expect("finish compression")
}

fn with_stream(dictionary: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dictionary} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

/// The trailer names catalog 10, with two pages. Catalog 1, with one page,
/// is written after it. Only a cross-reference stream describes the file.
fn described_by_a_stream_only(flate: bool) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut objects = second_page();
    objects.extend([
        (3, page(11, 4)),
        (4, text("One")),
        (9, FONT.to_string()),
        (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
        (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
    ]);
    let mut offsets = write_objects(&mut out, &objects);
    let xref = out.len();
    offsets.push((30, xref));
    let mut index = String::new();
    let mut rows = Vec::new();
    for (object, offset) in offsets {
        index.push_str(&format!("{object} 1 "));
        rows.push(1u8);
        rows.extend_from_slice(&u32::try_from(offset).expect("small file").to_be_bytes());
        rows.push(0);
    }
    let (filter, data) = if flate {
        (" /Filter /FlateDecode", zlib(&rows))
    } else {
        ("", rows)
    };
    out.extend_from_slice(b"30 0 obj\n");
    out.extend_from_slice(&with_stream(
        &format!("/Type /XRef /Size 100 /W [1 4 1] /Index [{index}] /Root 10 0 R{filter}"),
        &data,
    ));
    out.extend_from_slice(format!("\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

#[test]
fn a_cross_reference_stream_names_the_catalog_whatever_the_options() {
    // Compressed and not, with decoded streams kept and not (the language
    // bindings do not keep them). The last catalog in the file is the
    // one-page one, so "take the last catalog" fails all four.
    for flate in [true, false] {
        for keep in [true, false] {
            let options = ParseOptions {
                keep_decoded_streams: keep,
                ..ParseOptions::default()
            };
            let document = PdfDocument::from_slice(&described_by_a_stream_only(flate), options)
                .expect("parses");
            assert_eq!(
                document.pages().len(),
                2,
                "flate {flate}, keep_decoded_streams {keep}"
            );
        }
    }
}

/// A two-page file whose second revision adds nothing and marks `freed`
/// free.
fn with_a_free_entry(freed: u32) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let first = revision(
        &mut out,
        &[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
            (
                2,
                "<< /Type /Pages /Kids [3 0 R 12 0 R] /Count 2 >>".to_string(),
            ),
            (3, page(2, 4)),
            (4, text("One")),
            (12, page(2, 13)),
            (13, text("Two")),
            (9, FONT.to_string()),
        ],
        1,
        None,
        None,
    );
    let table = out.len();
    out.extend_from_slice(
        format!(
            "xref\n{freed} 1\n0000000000 00001 f \ntrailer\n<< /Size 100 /Root 1 0 R /Prev {first} >>\n\
             startxref\n{table}\n%%EOF\n"
        )
        .as_bytes(),
    );
    out
}

#[test]
fn a_free_entry_changes_nothing_that_is_still_referenced() {
    // The renderer ignores free entries, so extraction must too: a content
    // stream or a page a later table calls free, but the page tree still
    // points at, is still part of the document.
    for freed in [13, 12, 2] {
        let document = open(&with_a_free_entry(freed));
        assert_eq!(document.pages().len(), 2, "object {freed} freed");
        let words = words(&document);
        assert!(
            words.contains("One") && words.contains("Two"),
            "object {freed} freed: {words:?}"
        );
    }
}

#[cfg(feature = "rasterize")]
#[test]
fn extraction_and_the_interpreter_agree_about_a_freed_content_stream() {
    let blank = {
        let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        revision(
            &mut out,
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
                (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
                (3, page(2, 4)),
                (4, stream("")),
                (9, FONT.to_string()),
            ],
            1,
            None,
            None,
        );
        open(&out)
            .page_rasterized_png_interpreted_checked(0, 72.0)
            .expect("blank page renders")
    };
    let document = open(&with_a_free_entry(13));
    let drawn = document
        .page_rasterized_png_interpreted_checked(1, 72.0)
        .expect("renders");
    let has_text = words(&document).contains("Two");
    assert_eq!(
        drawn != blank,
        has_text,
        "the page is drawn exactly when its text is extracted"
    );
    assert!(has_text);
}

#[test]
fn a_hybrid_file_takes_packed_objects_from_its_cross_reference_stream() {
    // Object 2, the page tree, exists twice, each time inside a compressed
    // object stream: stream 5 holds a stale one-page tree, stream 7 the
    // live two-page tree. The classic table can only call object 2 free;
    // the `/XRefStm` section of the same trailer says it is in stream 7.
    // Both orders of the two streams are tried, so whichever copy scan
    // order prefers, one of the two files needs the chain to get it right.
    let packed = |number: u32, body: &str| {
        let data = format!("2 0 {body}");
        (
            number,
            with_stream(
                "/Type /ObjStm /N 1 /First 4 /Filter /FlateDecode",
                &zlib(data.as_bytes()),
            ),
        )
    };
    for live_first in [true, false] {
        let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = write_objects(
            &mut out,
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
                (3, page(2, 4)),
                (4, text("One")),
                (12, page(2, 13)),
                (13, text("Two")),
                (9, FONT.to_string()),
            ],
        );
        let mut streams = vec![
            packed(7, "<< /Type /Pages /Kids [3 0 R 12 0 R] /Count 2 >>"),
            packed(5, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        ];
        if !live_first {
            streams.reverse();
        }
        for (number, body) in streams {
            offsets.push((number, out.len()));
            out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let stream_offset = out.len();
        out.extend_from_slice(b"6 0 obj\n");
        out.extend_from_slice(&with_stream(
            "/Type /XRef /Size 100 /W [1 4 1] /Index [2 1]",
            &[2, 0, 0, 0, 7, 0],
        ));
        out.extend_from_slice(b"\nendobj\n");
        let table = out.len();
        out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n2 1\n0000000000 00000 f \n");
        for (number, offset) in offsets {
            out.extend_from_slice(format!("{number} 1\n{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size 100 /Root 1 0 R /XRefStm {stream_offset} >>\nstartxref\n{table}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let document = open(&out);
        assert_eq!(document.pages().len(), 2, "live stream first: {live_first}");
        assert!(words(&document).contains("Two"));
    }
}

/// How `two_copies` writes its table.
#[derive(Clone, Copy, PartialEq)]
enum Table {
    /// Well formed.
    Strict,
    /// Entries written as `offset 0 n`, not twenty bytes each.
    LooseEntries,
    /// Object 4's offset points at a comment written just before it.
    OffsetOnAComment,
    /// The font's offset is off by one; every other entry is right.
    OneWrongOffset,
    /// Other bytes before `xref` on its line.
    JunkBeforeKeyword,
    /// Object 4's only entry is free.
    OnlyEntryFree,
}

/// One page whose content stream, object 4, is written twice ("First", then
/// "Second"). The table names the first copy unless `second` is set.
fn two_copies(second: bool, table: Table) -> Vec<u8> {
    let word = |word: &str, x: u32| {
        stream(&format!(
            "{x} 40 30 30 re f BT /F1 12 Tf {x} 100 Td ({word}) Tj ET"
        ))
    };
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = write_objects(
        &mut out,
        &[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
            (3, page(2, 4)),
            (9, FONT.to_string()),
        ],
    );
    if table == Table::OneWrongOffset {
        if let Some(font) = offsets.last_mut() {
            font.1 += 1;
        }
    }
    let first = out.len();
    if table == Table::OffsetOnAComment {
        out.extend_from_slice(b"%pad\n");
    }
    write_objects(&mut out, &[(4, word("First", 20))]);
    let later = out.len();
    write_objects(&mut out, &[(4, word("Second", 120))]);
    let xref_line = out.len();
    if table == Table::JunkBeforeKeyword {
        out.extend_from_slice(b"junk ");
    }
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (number, offset) in offsets {
        let line = if table == Table::LooseEntries {
            format!("{number} 1\n{offset} 0 n\n")
        } else {
            format!("{number} 1\n{offset:010} 00000 n \n")
        };
        out.extend_from_slice(line.as_bytes());
    }
    let listed = if second { later } else { first };
    let entry = match table {
        Table::LooseEntries => format!("4 1\n{listed} 0 n\n"),
        Table::OnlyEntryFree => "4 1\n0000000000 00001 f \n".to_string(),
        _ => format!("4 1\n{listed:010} 00000 n \n"),
    };
    out.extend_from_slice(entry.as_bytes());
    let _ = xref_line;
    out.extend_from_slice(
        format!("trailer\n<< /Size 100 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

/// Which copy the text extraction read and which the interpreter drew.
#[cfg(feature = "rasterize")]
fn copies_used(bytes: &[u8]) -> (&'static str, &'static str) {
    let render = |bytes: &[u8]| {
        open(bytes)
            .page_rasterized_png_interpreted_checked(0, 72.0)
            .expect("renders")
            .expect("draws")
    };
    let first = render(&two_copies(false, Table::Strict));
    let second = render(&two_copies(true, Table::Strict));
    assert_ne!(first, second, "the reference renders must differ");
    let words = words(&open(bytes));
    let extracted = match (words.contains("First"), words.contains("Second")) {
        (true, false) => "first",
        (false, true) => "second",
        _ => "neither or both",
    };
    let png = render(bytes);
    let drawn = if png == first {
        "first"
    } else if png == second {
        "second"
    } else {
        "neither"
    };
    (extracted, drawn)
}

#[cfg(feature = "rasterize")]
#[test]
fn a_well_formed_table_picks_the_copy_for_text_and_picture_alike() {
    assert_eq!(
        copies_used(&two_copies(false, Table::Strict)),
        ("first", "first")
    );
    assert_eq!(
        copies_used(&two_copies(true, Table::Strict)),
        ("second", "second")
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn a_table_the_renderer_rejects_is_not_followed_for_text_either() {
    // The renderer throws its whole map away at the first thing it cannot
    // read and rebuilds by scanning, which finds the later copy. Following
    // the table for text alone showed "First" under a picture of "Second".
    for table in [Table::LooseEntries, Table::OffsetOnAComment] {
        let (extracted, drawn) = copies_used(&two_copies(false, table));
        assert_eq!(extracted, drawn, "text and picture read different copies");
        assert_eq!(extracted, "second");
    }
}

/// Known difference: the renderer discovers a bad offset only when it reads
/// that object, and rebuilds its map then. Here the bad entry is the
/// font's, which it reads after it has already fetched the content stream
/// through the table, so it draws the first copy. This crate cannot know
/// the order the renderer will read objects in; it discards the whole
/// chain for any bad entry and extracts the second.
#[cfg(feature = "rasterize")]
#[test]
#[ignore = "known difference from the renderer: it repairs its map lazily, in read order"]
fn one_wrong_offset_among_correct_ones_should_pick_the_same_copy_for_both() {
    let (extracted, drawn) = copies_used(&two_copies(false, Table::OneWrongOffset));
    assert_eq!(extracted, drawn);
}

/// Known difference: the renderer follows a table with other bytes before
/// `xref` on its line; this crate does not enter one there and falls back
/// to scan order, so the two read different copies.
#[cfg(feature = "rasterize")]
#[test]
#[ignore = "known difference from the renderer: table entry rule is stricter"]
fn a_table_after_junk_on_its_line_should_pick_the_same_copy_for_both() {
    let (extracted, drawn) = copies_used(&two_copies(false, Table::JunkBeforeKeyword));
    assert_eq!(extracted, drawn);
}

/// Known difference: an object whose only entry is free is not in the
/// renderer's map, so the page is drawn without it, but this crate's scan
/// still finds it and extracts its text.
#[cfg(feature = "rasterize")]
#[test]
#[ignore = "known difference from the renderer: free-only objects are extracted"]
fn an_object_whose_only_entry_is_free_should_be_neither_drawn_nor_extracted() {
    let document = open(&two_copies(false, Table::OnlyEntryFree));
    let drawn = document
        .page_rasterized_png_interpreted_checked(0, 72.0)
        .expect("renders");
    let words = words(&document);
    let reference = open(&two_copies(true, Table::Strict))
        .page_rasterized_png_interpreted_checked(0, 72.0)
        .expect("renders");
    assert_eq!(
        drawn == reference,
        words.contains("Second"),
        "text {words:?} does not match what was drawn"
    );
    assert!(drawn != reference && !words.contains("Second"));
}
