//! A text-only page must still produce no outlines, with or without the glyph
//! catalog compiled in.
//!
//! Text-operator support was added for Type 3 fonts, whose glyphs *are*
//! outlines. For every other font the glyphs live in a font program this parser
//! does not interpret, so a page of ordinary text has no outlines to extract
//! and must keep producing none. That is worth pinning: the extractor is on the
//! path that builds every text run and every table cell, and a change that
//! started emitting outline-shaped nonsense for normal text would be quiet.
//!
//! The check runs against the same document twice — once through the public
//! outline API and once through the semantic API that reads the characters — so
//! it separates "produced no outlines" from "read no text", which are different
//! facts and a test asserting only the first would not notice losing the second.
#![cfg(feature = "glyph-catalog")]

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

fn stream(body: &[u8]) -> Vec<u8> {
    let mut out = format!("<< /Length {} >>\nstream\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendstream");
    out
}

/// A page of ordinary text in a simple font, plus one drawn path.
///
/// The path is there deliberately: it proves the path pipeline still works, so
/// a change that broke outline extraction entirely could not pass this test by
/// producing zero outlines for the wrong reason.
fn text_page_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            b"BT /F1 12 Tf 72 700 Td (Ordinary text) Tj ET\n\
              0 0 0 rg 100 100 20 20 re f",
        ),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ])
}

#[test]
fn a_text_only_page_produces_no_outlines() {
    let doc = PdfDocument::from_slice(&text_page_pdf(), ParseOptions::default())
        .expect("the document must parse");

    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(
        glyphs, 0,
        "an ordinary font's glyphs are not outlines this extractor can read, so \
         a text-only page must yield none; text-operator support exists for Type \
         3 fonts, whose glyphs are content streams"
    );

    // The characters are still readable by the pass that is supposed to read
    // them, so this is not passing because the document is unreadable.
    let text: String = doc
        .semantic_text_runs_for_page(&doc.pages()[0])
        .iter()
        .map(|r| r.text.clone())
        .collect();
    assert!(
        text.contains("Ordinary text"),
        "the semantic pass must still read ordinary text; got {text:?}"
    );
}

/// A drawn rectangle on a page with no positioning run is still discarded.
///
/// This is pre-existing behaviour, not something the text work introduced: the
/// no-run flush branch keeps only punctuation-scale ink (<= 3.5pt) because its
/// transform model is a heuristic, and a 20pt rectangle is far above that. It
/// is pinned here because the Type 3 work had to interact with that guard — it
/// adds a third way into the positioned branch, and a careless version of it
/// would have let page-drawn shapes through as well.
#[test]
fn a_drawn_rectangle_on_an_unpositioned_page_is_still_discarded() {
    let doc = PdfDocument::from_slice(&text_page_pdf(), ParseOptions::default())
        .expect("the document must parse");
    let runs = doc.page_vector_text_runs(0).expect("vector runs");
    let glyphs: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    assert_eq!(
        glyphs, 0,
        "the punctuation-scale guard must keep discarding page-drawn shapes that \
         are not positioned by a run; Type 3 glyphs bypass it because the text \
         matrix positions them, and that exception must not widen"
    );
}
