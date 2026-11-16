//! Type 3 outline extraction, and the catalog recovering text through it.
//!
//! # The case
//!
//! A Type 3 font defines each glyph as a small content stream in `/CharProcs`
//! and shows it with the ordinary text operators. Nothing about the page's
//! content stream contains the outlines — they are one indirection away, behind
//! `Tf` and the font's encoding. When such a font also has no `/ToUnicode`, the
//! characters are not recoverable from the encoding either: the parser can
//! decode byte codes, but it cannot say whether the result is right. The
//! outlines are the only real evidence, which is what the glyph catalog consumes.
//!
//! # What the document looks like
//!
//! Two pages, each with its own font resource:
//!
//! * `/F1` has no `/ToUnicode`, so its text is flagged unverified.
//! * `/F2` maps the same codes through a CMap, so its text is decoded and can
//!   be correlated against the outlines of the glyphs that drew it.
//!
//! Both fonts share one `/CharProcs` pair, so the shapes learned from `/F2` are
//! the shapes `/F1` draws. That is the recovery the last test asserts.
//!
//! # What is covered where
//!
//! The catalog comparison itself is unit-tested in `glyph_catalog.rs` against the
//! same function the parser calls. This file covers the pipeline: that a text
//! operator produces outlines at all, that they reach the catalog, and that a
//! learned shape is found again through the parser's own lookup.

#![cfg(feature = "glyph-catalog")]

use pdf_fast_core::glyph_catalog;
use pdf_fast_core::vector_text::BezPt;
use pdf_fast_core::{ParseOptions, PdfDocument};

/// Assemble a PDF from object bodies, writing a real xref table.
///
/// The xref matters: this parser is byte-level and xref-free, but a fixture
/// whose object numbering does not line up produces a document that parses into
/// the wrong resource tree, which is a fixture bug that looks like a parser
/// bug. Every object here is written at a recorded offset.
/// The learned-shape catalog is one per process, and the test harness runs
/// tests on parallel threads: a test that clears it while another is
/// learning makes that one fail now and then. Reading glyph outlines
/// consults it too, so every test in this file holds this for its whole run.
fn catalog_lock() -> std::sync::MutexGuard<'static, ()> {
    static CATALOG: std::sync::Mutex<()> = std::sync::Mutex::new(());
    CATALOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pdf(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn stream(dict: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendstream");
    out
}

/// A square outline: one contour, four points.
fn square_proc() -> Vec<u8> {
    stream("", b"600 0 d0 0 0 m 600 0 l 600 600 l 0 600 l h f")
}

/// A T outline: two contours, so contour count alone distinguishes it.
fn tee_proc() -> Vec<u8> {
    stream(
        "",
        b"600 0 d0 0 500 m 600 500 l 600 600 l 0 600 l h f \
          250 0 m 350 0 l 350 500 l 250 500 l h f",
    )
}

/// A Type 3 font drawing `A` and `B` from 8 0 R and 9 0 R.
fn type3_font(tounicode: Option<&str>) -> Vec<u8> {
    let cmap = tounicode
        .map(|reference| format!(" /ToUnicode {reference}"))
        .unwrap_or_default();
    format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 600 600] \
         /FontMatrix [0.001 0 0 0.001 0 0] \
         /CharProcs << /square 6 0 R /tee 8 0 R >> \
         /Encoding << /Type /Encoding /Differences [65 /square 66 /tee] >> \
         /FirstChar 65 /LastChar 66 /Widths [600 600]{cmap} >>"
    )
    .into_bytes()
}

/// A CMap mapping 65 -> A and 66 -> B, so text drawn with it is decoded and
/// therefore usable as correlation ground truth.
fn tounicode_cmap() -> Vec<u8> {
    let body = b"/CIDInit /ProcSet findresource begin\n\
12 dict begin\nbegincmap\n\
/CMapName /Custom def\n/CMapType 2 def\n\
1 begincodespacerange\n<41> <42>\nendcodespacerange\n\
2 beginbfchar\n<41> <0041>\n<42> <0042>\nendbfchar\n\
endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend";
    stream("", body)
}

/// Two pages: page one's `/F2` decodes through a CMap, page two's `/F1` does
/// not. Both draw the same two glyphs from the same `/CharProcs`.
fn two_page_pdf() -> Vec<u8> {
    // Object numbers are the vector index + 1. Written out here because a
    // mismatch between a reference and an index produces a document that parses
    // into the wrong resource tree — a fixture bug that looks like a parser bug,
    // which cost real debugging time once already.
    pdf(vec![
        // 1 catalog
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        // 2 pages
        b"<< /Type /Pages /Kids [3 0 R 9 0 R] /Count 2 >>".to_vec(),
        // 3 page one, drawing with font 5
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        // 4 page one content
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
        // 5 font one: same glyphs, with a CMap so its text is decoded
        type3_font(Some("7 0 R")),
        // 6 the two glyph procedures both fonts share
        square_proc(),
        // 7 the CMap
        tounicode_cmap(),
        // 8 the second glyph procedure
        tee_proc(),
        // 9 page two, drawing with font 11
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 11 0 R >> >> /Contents 10 0 R >>"
            .to_vec(),
        // 10 page two content
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
        // 11 font two: the same glyphs, no CMap
        type3_font(None),
    ])
}

/// One page drawing `(A)` then `[(A) -2000 (B)] TJ`.
fn tj_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 12 Tf 72 700 Td (A) Tj [(A) -2000 (B)] TJ ET"),
        type3_font(Some("7 0 R")),
        square_proc(),
        tounicode_cmap(),
        tee_proc(),
    ])
}

/// One page drawing with the `'` and `"` operators, which show a string after
/// moving to the next line.
fn quote_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 12 Tf 72 700 Td (A) ' (B) \" ET"),
        type3_font(Some("7 0 R")),
        square_proc(),
        tounicode_cmap(),
        tee_proc(),
    ])
}

#[test]
fn type3_outlines_are_extracted_and_their_text_is_recovered_by_shape() {
    let _catalog = catalog_lock();
    glyph_catalog::clear_learned();
    let doc = PdfDocument::from_slice(&two_page_pdf(), ParseOptions::default())
        .expect("the Type 3 document must parse");

    let pages = doc.pages();
    assert_eq!(pages.len(), 2, "the fixture declares two pages");

    // Step 1: text operators are read and glyph outlines are built. Before this
    // path existed the count here was zero, which is exactly what an operator
    // table of path and graphics state implies.
    let runs = doc
        .page_vector_text_runs(0)
        .expect("vector runs must extract");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(
        glyphs, 2,
        "the page shows two glyphs, drawn from /CharProcs"
    );

    // Step 2: they carry geometry, without which the catalog has nothing to key
    // on even once outlines exist.
    let with_geometry = runs
        .iter()
        .flat_map(|r| r.glyphs.iter())
        .filter(|g| !g.normalized.is_empty())
        .count();
    assert_eq!(with_geometry, glyphs, "every glyph must carry geometry");

    // Step 3: the semantic pass reads the characters, flagged as unverified —
    // there is no `/ToUnicode`, so byte-code fallback is all there is.
    let decoded: String = doc
        .semantic_text_runs_for_page(&pages[1])
        .iter()
        .map(|r| r.text.clone())
        .collect();
    assert_eq!(decoded, "AB", "the text operators must resolve");
    let fallback: u32 = doc
        .semantic_text_runs_for_page(&pages[1])
        .iter()
        .map(|r| r.cmap_fallback_chars)
        .sum();
    assert_eq!(
        fallback as usize,
        decoded.chars().count(),
        "every character must be reported as unverified, which is the signal shape recovery replaces"
    );

    // Step 4: the learner correlates outlines with decoded characters.
    let stats = pdf_fast_core::vector_text::learn_document_glyph_mappings(&doc);
    assert!(
        stats.emitted > 0,
        "the learner must correlate outlines with decoded text: {stats:?}"
    );
    assert!(
        glyph_catalog::learned_shape_count() > 0,
        "correlation must leave the catalog holding shapes"
    );

    // Step 5: the text is read back through the shapes learned in step 4. This
    // is the whole point of the exercise — the page has no encoding path of its
    // own, so anything recovered here came from the outlines.
    let runs = doc.page_vector_text_runs(1).expect("page two outlines");
    let recovered = pdf_fast_core::vector_text::runs_to_string(&runs);
    let known: String = recovered.chars().filter(|c| *c != '_').collect();
    assert!(
        known.contains('A') || known.contains('B'),
        "page two's glyphs must be recovered by shape; got {recovered:?}"
    );

    glyph_catalog::clear_learned();
}

/// The wiring, on a document that does produce outlines.
///
/// `page_vector_text_as_string_checked` already refuses to return a string of
/// placeholders — it answers `None` when nothing was resolved. This asserts
/// that property holds with an empty catalog, so a later recovery cannot be
/// confused with a fixture that was readable all along.
#[test]
fn an_empty_catalog_reports_unknown_rather_than_guessing() {
    let _catalog = catalog_lock();
    glyph_catalog::clear_learned();
    let doc = PdfDocument::from_slice(&two_page_pdf(), ParseOptions::default())
        .expect("the outline document must parse");

    let runs = doc
        .page_vector_text_runs(0)
        .expect("vector runs must extract");
    assert!(
        !runs.is_empty(),
        "the fixture must draw outlines, or this test asserts nothing"
    );

    let text = doc.page_vector_text_as_string_checked(0).unwrap();
    assert!(
        text.as_deref()
            .map(|t| t.chars().all(|c| c == '_' || c == '\n' || c == ' '))
            .unwrap_or(true),
        "with an empty catalog every glyph is unknown; got {text:?}"
    );
}

/// A mapping learned through the parser's own learner is found again by the
/// parser's own lookup.
///
/// This is the defect end to end: the learner always recorded something and the
/// lookup never found it, because both were keyed on a hash that float noise
/// invalidates. The test learns from one rendering and resolves a different
/// rendering of the same shape.
#[test]
fn a_shape_learned_from_outlines_is_recovered_for_a_different_rendering() {
    let _catalog = catalog_lock();
    glyph_catalog::clear_learned();

    let outline = |jitter: f32| {
        vec![vec![
            BezPt {
                x: jitter,
                y: 0.0,
                kind: 1,
            },
            BezPt {
                x: 0.5,
                y: 0.30,
                kind: 1,
            },
            BezPt {
                x: 1.0,
                y: 0.0,
                kind: 1,
            },
        ]]
    };

    glyph_catalog::record_learned(
        0xa110_00bb,
        &outline(0.0),
        "A",
        12.0,
        0.9,
        glyph_catalog::CatalogSource::Corpus,
    );
    assert_eq!(glyph_catalog::learned_shape_count(), 1);

    // A later rendering: same shape, different hash. The old lookup returned
    // None here even though the shape had been learned.
    let recovered = glyph_catalog::lookup_entry(
        0x0000_0000,
        &outline(0.002),
        12.0,
        glyph_catalog::DEFAULT_EPS,
    );
    assert_eq!(
        recovered.as_deref(),
        Some("A"),
        "a shape learned from outlines must survive a re-rendering"
    );

    glyph_catalog::clear_learned();
}

/// A `TJ` array is shown in order, with its kerning applied.
///
/// `TJ` interleaves strings with numbers: the strings are shown, and each number
/// between them shifts the text matrix by thousandths of an em before the next
/// string. Treating the array as a run of strings would draw every glyph in the
/// wrong place; the earlier behaviour — skipping the array — draws nothing at
/// all. The assertion is on the *gap* between the two glyphs rather than on the
/// count, because a count passes with the kerning ignored.
#[test]
fn a_tj_array_shows_every_string_with_its_kerning_applied() {
    let _catalog = catalog_lock();
    glyph_catalog::clear_learned();
    let doc = PdfDocument::from_slice(&tj_pdf(), ParseOptions::default())
        .expect("the document must parse");

    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: Vec<&pdf_fast_core::vector_text::VectorGlyph> =
        runs.iter().flat_map(|r| r.glyphs.iter()).collect();
    // Three in total: the standalone `(A) Tj` before the array, then the two
    // strings inside it.
    assert_eq!(
        glyphs.len(),
        3,
        "every string must be shown, both the standalone one and both in the array"
    );

    // Plain advance is 600 units at 12pt = 7.2pt. The -2000 adjustment adds
    // 24pt, so the threshold sits unambiguously between the two.
    let gap = (glyphs[2].x_offset - glyphs[1].x_offset).abs();
    assert!(
        gap > 10.0,
        "the kerning operand must move the second glyph well past the plain \
         advance; a gap near 7.2 means the numbers between the strings were \
         ignored: {gap}"
    );

    glyph_catalog::clear_learned();
}

/// `'` and `"` show their string, so their glyphs are drawn too.
#[test]
fn the_quote_operators_show_their_string() {
    let _catalog = catalog_lock();
    glyph_catalog::clear_learned();
    let doc = PdfDocument::from_slice(&quote_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(glyphs, 2, "both quote operators must draw their string");
    glyph_catalog::clear_learned();
}
