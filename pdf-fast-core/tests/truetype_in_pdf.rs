//! An embedded TrueType font yields outlines for the atlas.
//!
//! This is the case the atlas was blind to: a document embedding a font
//! program, which is what anything from a word processor does. Real documents
//! on this machine embed 8-10 `/FontFile2` subsets each, and until the `glyf`
//! reader landed, every one of them produced zero outlines.
//!
//! The fixture embeds a real TrueType subset and draws text with it, so the
//! whole chain is exercised: `/FontFile2` is found on the descriptor, parsed,
//! the character code mapped through `cmap` to a glyph, the outline emitted,
//! and placed by the text matrix.

#![cfg(feature = "glyph-atlas")]

use pdf_fast_core::{ParseOptions, PdfDocument};

const FIXTURE: &[u8] = include_bytes!("fixtures/DejaVuSans-subset.ttf");

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

/// Catalog, pages, page, content, font, descriptor, program — objects 1..=7.
///
/// The font is a simple TrueType with `/Widths`, which is the direct route; a
/// Type0/descendant font reaches the same descriptor through
/// `/DescendantFonts`, and `font_descriptor` follows that too.
fn embedded_truetype_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 24 Tf 72 700 Td (AB) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /DejaVuSans \
          /FirstChar 65 /LastChar 66 /Widths [1401 1367] \
          /FontDescriptor 6 0 R /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /DejaVuSans /Flags 32 \
          /FontBBox [-1021 -463 1793 1232] /ItalicAngle 0 /Ascent 1556 \
          /Descent -492 /CapHeight 1493 /StemV 80 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", FIXTURE),
    ])
}

/// The glyphs are drawn from the embedded program.
#[test]
fn an_embedded_program_yields_glyph_outlines() {
    let doc = PdfDocument::from_slice(&embedded_truetype_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: Vec<&pdf_fast_core::vector_text::VectorGlyph> =
        runs.iter().flat_map(|r| r.glyphs.iter()).collect();

    assert_eq!(
        glyphs.len(),
        2,
        "both characters must be drawn from the embedded program"
    );
    assert!(
        glyphs.iter().all(|g| !g.normalized.is_empty()),
        "each glyph must carry normalized geometry, or the atlas has nothing to key on"
    );
}

/// The glyphs are placed by the font's own metrics, not stacked at one point.
///
/// Without `hmtx` every advance is zero and both glyphs land together — shapes
/// correct, positions wrong, which the outline assertion above cannot see.
#[test]
fn embedded_glyphs_advance_by_their_widths() {
    let doc = PdfDocument::from_slice(&embedded_truetype_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: Vec<&pdf_fast_core::vector_text::VectorGlyph> =
        runs.iter().flat_map(|r| r.glyphs.iter()).collect();

    let gap = (glyphs[1].x_offset - glyphs[0].x_offset).abs();
    assert!(
        gap > 1.0,
        "the second glyph must advance past the first; a near-zero gap means the \
         advance widths were not read: {gap}"
    );
}

/// The outline is the font's real shape, scaled by the em size.
///
/// `A` spans 1493 font units of cap height in a 2048-unit em, and the page asks
/// for 24pt, so the drawn height is about 24 * 1493/2048 = 17.5pt. Asserting a
/// plausible range rather than an exact value keeps this robust to rounding
/// while still failing if the font matrix were wrong by orders of magnitude —
/// which is the real risk, since a missing `unitsPerEm` would scale by 1
/// instead of 1/2048.
#[test]
fn the_outline_is_scaled_by_the_em_size() {
    let doc = PdfDocument::from_slice(&embedded_truetype_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let first = &runs
        .iter()
        .flat_map(|r| r.glyphs.iter())
        .next()
        .expect("at least one glyph");

    assert!(
        (10.0..30.0).contains(&first.size),
        "a 24pt 'A' should measure roughly 17.5pt tall; got {}",
        first.size
    );
}

/// A document with no embedded program still yields nothing rather than
/// inventing outlines from the metrics.
#[test]
fn a_font_without_a_program_yields_no_outlines() {
    let bytes = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 24 Tf 72 700 Td (AB) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /Helvetica \
          /FontDescriptor 6 0 R /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /Helvetica /Flags 32 >>".to_vec(),
    ]);
    let doc = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(
        glyphs, 0,
        "without an embedded program there is no outline to read, and the atlas \
         must get nothing rather than a shape derived from the widths"
    );
}

/// An embedded CFF program yields outlines too.
///
/// OpenType fonts produced by design tools and by LaTeX wrap CFF rather than
/// `glyf`, so a PDF embedding one gives the atlas nothing until the charstrings
/// are read. This is the third route to the same outlines: Type 3 glyphs are
/// content streams, `glyf` glyphs are a table, CFF glyphs are a bytecode.
///
/// The fixture is a Cabin Regular subset (SIL Open Font License).
#[test]
fn an_embedded_cff_program_yields_glyph_outlines() {
    const CFF_FIXTURE: &[u8] = include_bytes!("fixtures/Cabin-subset.otf");
    // Objects 1..=7: catalog, pages, page, content, font, descriptor, program.
    let bytes = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 24 Tf 72 700 Td (AB) Tj ET"),
        // An OpenType subtype: `/FontFile3` carries the program.
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Cabin-Regular \
          /FirstChar 65 /LastChar 66 /Widths [600 600] \
          /FontDescriptor 6 0 R /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /Cabin-Regular /Flags 32 \
          /FontBBox [-430 -608 2518 2276] /ItalicAngle 0 /Ascent 1556 \
          /Descent -492 /CapHeight 1400 /StemV 80 /FontFile3 7 0 R >>"
            .to_vec(),
        stream("/Subtype /OpenType", CFF_FIXTURE),
    ]);
    let doc = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: Vec<&pdf_fast_core::vector_text::VectorGlyph> =
        runs.iter().flat_map(|r| r.glyphs.iter()).collect();

    assert_eq!(
        glyphs.len(),
        2,
        "both characters must be drawn from the embedded CFF program"
    );
    assert!(
        glyphs.iter().all(|g| !g.normalized.is_empty()),
        "each glyph must carry normalized geometry for the atlas"
    );
    let gap = (glyphs[1].x_offset - glyphs[0].x_offset).abs();
    assert!(
        gap > 1.0,
        "the second glyph must advance past the first; a near-zero gap means the \
         charstring widths were not read: {gap}"
    );
}
