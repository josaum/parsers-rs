//! An array written as an indirect object must be reachable.
//!
//! A producer that shares or sizes a long array writes the array itself as the
//! object — `/Widths 12 0 R` across subset fonts, `/Differences` when it is
//! long. Only dictionaries used to be kept when an object was parsed, so such a
//! reference resolved to nothing. For a font that means no advance widths, and
//! the symptom is subtle: glyphs draw, but every one of them advances by zero
//! and the text collapses onto a single position.
//!
//! The test drives it through the Type 3 path, where both the widths and the
//! encoding names are indirect, and asserts the glyphs land at *different* x
//! positions. Asserting only that outlines appear would pass with the arrays
//! missing, because outlines do not depend on widths.

use pdf_fast_core::{ParseOptions, PdfDocument};

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

/// Objects 1..=4 are catalog, pages, page, content; 5 is the font; 6 and 7 are
/// the glyph procedures; 8 is `/Widths` and 9 `/Differences`, both indirect.
fn indirect_arrays_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"BT /F1 12 Tf 72 700 Td (AB) Tj ET"),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 600 600] \
          /FontMatrix [0.001 0 0 0.001 0 0] \
          /CharProcs << /square 6 0 R /tee 7 0 R >> \
          /Encoding << /Type /Encoding /Differences 9 0 R >> \
          /FirstChar 65 /LastChar 66 /Widths 8 0 R >>"
            .to_vec(),
        stream("", b"600 0 d0 0 0 m 600 0 l 600 600 l 0 600 l h f"),
        stream(
            "",
            b"600 0 d0 0 500 m 600 500 l 600 600 l 0 600 l h f \
              250 0 m 350 0 l 350 500 l 250 500 l h f",
        ),
        b"[600 600]".to_vec(),
        b"[65 /square 66 /tee]".to_vec(),
    ])
}

#[test]
fn an_indirect_widths_array_still_advances_the_text() {
    let doc = PdfDocument::from_slice(&indirect_arrays_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");

    let glyphs: Vec<&pdf_fast_core::vector_text::VectorGlyph> =
        runs.iter().flat_map(|r| r.glyphs.iter()).collect();
    assert_eq!(
        glyphs.len(),
        2,
        "both glyphs must be drawn, so a zero-width result cannot be mistaken \
         for a parse failure"
    );

    // Widths of 600 at 12pt are 7.2pt apart, so the two glyphs sit at
    // different x offsets. With the indirect array unreachable every width
    // reads as zero and both land on the same spot.
    let xs: Vec<f32> = glyphs.iter().map(|g| g.x_offset).collect();
    assert!(
        (xs[0] - xs[1]).abs() > 1.0,
        "the second glyph must advance past the first; identical offsets mean \
         /Widths 8 0 R resolved to nothing: {xs:?}"
    );
}

#[test]
fn an_indirect_differences_array_still_maps_the_codes() {
    let doc = PdfDocument::from_slice(&indirect_arrays_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(
        glyphs, 2,
        "with /Differences 9 0 R unreachable no code names a glyph, so nothing \
         would be drawn at all"
    );
}
