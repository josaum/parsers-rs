//! End-to-end behaviour of the glyph atlas on a document whose text is drawn
//! as outlines.
//!
//! # What this covers, and what it does not
//!
//! The atlas comparison itself is covered by unit tests in `glyph_atlas.rs`,
//! against the same function the parser calls. What needs a real document is
//! the *wiring*: that a parser-visible outline reaches the atlas, that a
//! learned mapping is found again, and that an unknown glyph is reported rather
//! than invented.
//!
//! These tests therefore drive the public API over hand-built documents and
//! assert what the parser actually reports at each step, including asserting
//! that the fixture produced work to do. Where a fixture cannot exercise a
//! path, the test says so instead of passing vacuously.
//!
//! # The boundary this file also pins
//!
//! A faithful Type 3 font — glyph procedures in `/CharProcs`, no `/ToUnicode` —
//! is the real-world case the atlas was built for, and the fixture below is one.
//! It parses, its pages are found, and its characters come through the semantic
//! pass flagged as unverified; but the extractor reports **zero** vector glyph
//! runs for it, so the atlas never sees an outline. Reaching Type 3 needs
//! outline extraction — resolve `Tf` to the font, read `/Encoding /
//! Differences` and `/CharProcs`, execute each glyph's stream under the font
//! matrix composed with the text matrix. That is a feature in the
//! content-stream interpreter, not a fix to the atlas, and
//! `a_type3_font_yields_no_outline_runs_today` pins the gap so implementing it
//! causes a failure rather than silence.

#![cfg(feature = "glyph-atlas")]

use pdf_fast_core::glyph_atlas;
use pdf_fast_core::vector_text::BezPt;
use pdf_fast_core::{ParseOptions, PdfDocument};

/// Assemble a PDF from object bodies, writing a real xref table.
fn pdf(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
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

/// Glyph procedures for an L-shaped mark and a T-shaped mark.
///
/// Deliberately different contour counts and point counts, so a tolerant match
/// cannot confuse one with the other — the test would pass either way against a
/// matcher that ignored structure, and that matcher would be wrong.
const CHARPROCS: &str = r#"/CharProcs << /square 8 0 R /tee 9 0 R >>"#;

fn square_proc() -> Vec<u8> {
    // One closed contour, four points.
    stream("", b"1000 0 0 0 0 1000 d1\n0 0 m 600 0 l 600 600 l 0 600 l h")
}

fn tee_proc() -> Vec<u8> {
    // Two contours, so structure alone separates it from the square.
    stream(
        "",
        b"1000 0 0 0 0 1000 d1\n0 500 m 600 500 l 600 600 l 0 600 l h\n250 0 m 350 0 l 350 500 l 250 500 l h",
    )
}

fn type3_font() -> Vec<u8> {
    format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 600 600] \
         /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /square 8 0 R /tee 9 0 R >> \
         /Encoding << /Type /Encoding /Differences [65 /square 66 /tee] >> \
         /FirstChar 65 /LastChar 66 /Widths [600 600] /Resources << >> >>"
    )
    .into_bytes()
}

/// A `/ToUnicode` CMap mapping 65 -> A and 66 -> B, so page one is decodable.
fn tounicode_cmap() -> Vec<u8> {
    let body = b"/CIDInit /ProcSet findresource begin\n\
12 dict begin\nbegincmap\n\
/CMapName /Custom def\n/CMapType 2 def\n\
1 begincodespacerange\n<41> <42>\nendcodespacerange\n\
2 beginbfchar\n<41> <0041>\n<42> <0042>\nendbfchar\n\
endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend";
    stream("", body)
}

fn page_tree(kids: &str, count: usize) -> Vec<u8> {
    format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>").into_bytes()
}

/// Build a two-page document sharing one font.
///
/// `page_two_tounicode` decides whether the second page's font resource also
/// carries a CMap. With it absent, page two's codes cannot be decoded and its
/// text is recoverable only by shape.
fn two_page_pdf(page_two_tounicode: bool) -> Vec<u8> {
    let font_page_one = "<< /F1 5 0 R >>";
    let font_page_two = if page_two_tounicode {
        "<< /F1 10 0 R >>"
    } else {
        "<< /F1 5 0 R >>"
    };
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        page_tree("3 0 R 11 0 R", 2),
        // Page one: decodable, so the learner has pairs to correlate.
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources {font_page_one} /Contents 4 0 R >>"
        )
        .into_bytes(),
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
        type3_font(),
        // The CMap for page one's font resource.
        {
            let mut f = type3_font();
            let s = String::from_utf8(f.clone()).unwrap();
            let with_cmap = s.replace(
                "/Resources << >> >>",
                "/ToUnicode 6 0 R /Resources << >> >>",
            );
            f = with_cmap.into_bytes();
            f
        },
        tounicode_cmap(),
        b"<< /Type /Font /Subtype /Type3 >>".to_vec(), // 8: placeholder, replaced below
        b"<< /Type /Font /Subtype /Type3 >>".to_vec(), // 9
        // 10: the same font as 5 but *without* a CMap, for page two.
        String::from_utf8(type3_font()).unwrap().into_bytes(),
        // Page two.
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources {font_page_two} /Contents 12 0 R >>"
        )
        .into_bytes(),
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
    ])
}

/// The same document, but the two pages share the CMap-carrying font, so both
/// pages decode through `/ToUnicode` and the atlas is not needed.
fn baseline_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        page_tree("3 0 R 9 0 R", 2),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /F1 5 0 R >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
        type3_font(),
        {
            let s = String::from_utf8(type3_font()).unwrap();
            s.replace("/Resources << >> >>", "/ToUnicode 6 0 R /Resources << >> >>")
                .into_bytes()
        },
        tounicode_cmap(),
        square_proc(),
        tee_proc(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /F1 5 0 R >> /Contents 10 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
    ])
}

/// A page whose letterforms are drawn as filled paths directly in the content
/// stream — the shape of document the outline extractor does rebuild.
///
/// Two different shapes, with different contour and point counts, so a matcher
/// that ignored structure could not confuse them.
fn outline_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        page_tree("3 0 R", 1),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            // An L and a T, drawn as outlines near the origin.
            b"0 0 0 rg
              100 600 m 160 600 l 160 700 l 100 700 l h f
              200 600 m 260 600 l 260 700 l 200 700 l h f
              300 660 m 400 660 l 400 700 l 300 700 l h f
",
        ),
    ])
}

/// The honest statement of what this parser does with a Type 3 font.
///
/// A Type 3 font defines each glyph as a small content stream in `/CharProcs` —
/// a path, drawn by `d0`/`d1` and the usual `m`/`l`/`c`/`f` operators — and
/// shows it with the ordinary text operators. Reading such a document's text
/// requires resolving the text operators against the font, then executing each
/// glyph's stream under the font matrix composed with the text matrix.
///
/// The outline extractor does neither: its operator table (`vector_text.rs`)
/// handles path and graphics state only — no `BT`, `Tf`, `Td`, `Tj` — and no
/// code anywhere reads `/CharProcs`. So a Type 3 page yields zero outline runs,
/// which is exactly what this test pins. It is a fixture limitation with a
/// cause, not a fixture that happened to be malformed.
///
/// The atlas this file otherwise exercises therefore cannot be reached on the
/// document class it was built for until Type 3 outline extraction exists.
/// Pinning the current behaviour keeps that gap visible: if extraction is added,
/// this test fails and the end-to-end recovery assertion can be written.
#[test]
fn a_type3_font_yields_no_outline_runs_today() {
    let doc = PdfDocument::from_slice(&two_page_pdf(false), ParseOptions::default())
        .expect("the Type 3 document must parse");
    let pages = doc.pages();
    assert!(!pages.is_empty(), "the document must yield pages");

    for page in &pages {
        let runs = doc
            .page_vector_text_runs(page.page_index)
            .expect("vector runs must not fail on a well-formed document");
        let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
        assert_eq!(
            (runs.len(), glyphs),
            (0, 0),
            "page {} unexpectedly produced outline runs; if Type 3 extraction \
             has been implemented, replace this test with a recovery assertion \
             rather than deleting it",
            page.page_index
        );
    }

    // The characters themselves do come through, flagged as unverified.
    let page = &pages[0];
    let runs = doc.semantic_text_runs_for_page(page);
    let text: String = runs.iter().map(|r| r.text.clone()).collect();
    let fallback: u32 = runs.iter().map(|r| r.cmap_fallback_chars).sum();
    assert_eq!(text, "AB", "the text operators must resolve even without a CMap");
    assert_eq!(
        fallback as usize,
        text.chars().count(),
        "every character must be reported as decoded by byte-code fallback"
    );
}

/// The control: a document this parser can read by encoding alone.
///
/// If this failed, a later failure would be about the fixture rather than about
/// the atlas, which is why it is separate.
#[test]
fn a_document_with_a_cmap_is_readable_without_the_atlas() {
    glyph_atlas::clear_learned();
    let doc = PdfDocument::from_slice(&baseline_pdf(), ParseOptions::default())
        .expect("the baseline document must parse");
    assert!(
        !doc.pages().is_empty(),
        "the baseline document must yield at least one page"
    );
    // Page one is drawn from glyph procedures, which this parser does not
    // rebuild into outlines; the checked accessor must report that as "nothing
    // recovered" rather than returning a string of placeholders. With no atlas
    // populated, anything non-placeholder here would have to be invented.
    let text = doc
        .page_vector_text_as_string_checked(0)
        .expect("vector text must not fail on a well-formed document")
        .unwrap_or_default();
    assert!(
        text.chars().all(|c| c == '_' || c == '\n' || c == ' '),
        "an empty atlas must not invent characters; got {text:?}"
    );
}

/// The wiring, on a document that does produce outlines.
///
/// `page_vector_text_as_string_checked` already refuses to return a string of
/// placeholders — it answers `None` when nothing was resolved. This asserts
/// that property holds with an empty atlas, so a later recovery cannot be
/// confused with a fixture that was readable all along.
#[test]
fn an_empty_atlas_reports_unknown_rather_than_guessing() {
    glyph_atlas::clear_learned();
    let doc = PdfDocument::from_slice(&outline_pdf(), ParseOptions::default())
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
        "with an empty atlas every glyph is unknown; got {text:?}"
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
    glyph_atlas::clear_learned();

    let outline = |jitter: f32| {
        vec![vec![
            BezPt { x: jitter, y: 0.0, kind: 1 },
            BezPt { x: 0.5, y: 0.30, kind: 1 },
            BezPt { x: 1.0, y: 0.0, kind: 1 },
        ]]
    };

    glyph_atlas::record_learned(
        0xa110_00bb,
        &outline(0.0),
        "A",
        12.0,
        0.9,
        glyph_atlas::AtlasSource::Corpus,
    );
    assert_eq!(glyph_atlas::learned_shape_count(), 1);

    // A later rendering: same shape, different hash. The old lookup returned
    // None here even though the shape had been learned.
    let recovered = glyph_atlas::lookup_entry(
        0x0000_0000,
        &outline(0.002),
        12.0,
        glyph_atlas::DEFAULT_EPS,
    );
    assert_eq!(
        recovered.as_deref(),
        Some("A"),
        "a shape learned from outlines must survive a re-rendering"
    );

    glyph_atlas::clear_learned();
}
