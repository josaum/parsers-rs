//! Inline image payloads must not become text.
//!
//! `BI <params> ID <binary> EI` is the one place a content stream carries raw
//! bytes that are not operands. Without handling, the tokenizer reads the
//! bitmap as operators and strings: measured on a fixture whose 1x1 RGB payload
//! happened to contain `(AB)Tj`, the extracted text came back as
//! `BeforeABAfter` — three characters of fabricated text from image data.
//!
//! Fabricated text is the worst outcome available here. Missing text is visible
//! to a reader; invented text is not, and it flows into whatever the extraction
//! feeds.
//!
//! These tests assert both halves: the surrounding text survives, and no
//! payload bytes appear in it. Asserting only that the payload is absent would
//! pass if the whole stream were dropped.

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

fn content_stream(body: &[u8]) -> Vec<u8> {
    let mut out = format!("<< /Length {} >>\nstream\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendstream");
    out
}

fn document(content: &[u8]) -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        content_stream(content),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
          /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ])
}

fn text_of(bytes: &[u8]) -> String {
    let doc = PdfDocument::from_slice(bytes, ParseOptions::default()).expect("must parse");
    let page = &doc.pages()[0];
    doc.semantic_text_runs_for_page(page)
        .iter()
        .map(|r| r.text.clone())
        .collect()
}

/// The failing case: an unfiltered payload whose bytes look like operators.
#[test]
fn an_unfiltered_payload_that_looks_like_text_does_not_become_text() {
    // 4x1 RGB at 8bpc = 12 bytes, one of which spells `(AB)Tj`.
    let content = b"BT /F1 12 Tf 72 700 Td (Before) Tj ET\n\
        BI /W 4 /H 1 /CS /RGB /BPC 8 ID\n\
        \x00\x28\x41\x42\x29\x54\x6a\x20\x5d\x42\x44\x00\n\
        EI\n\
        BT /F1 12 Tf 72 680 Td (After) Tj ET";
    let text = text_of(&document(content));

    assert!(
        text.contains("Before") && text.contains("After"),
        "the text on both sides of the image must survive; got {text:?}"
    );
    assert!(
        !text.contains("AB") && !text.contains('T') && !text.contains(']'),
        "no payload byte may reach the text; got {text:?}"
    );
}

/// The geometry is used, so a payload containing the letters `EI` does not end
/// the skip early — which would resume tokenizing mid-payload.
#[test]
fn a_payload_containing_ei_does_not_end_the_skip_early() {
    // 4x3 RGB at 8bpc = 36 bytes; `EI` appears in the middle.
    let content = b"BT /F1 12 Tf 72 700 Td (Start) Tj ET\n\
        BI /W 4 /H 3 /CS /RGB /BPC 8 ID\n\
        \x01\x02EI\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\
\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\x20\x21\n\
        EI\n\
        BT /F1 12 Tf 72 680 Td (End) Tj ET";
    let text = text_of(&document(content));

    assert!(
        text.contains("Start") && text.contains("End"),
        "text after a payload containing `EI` must still be read; got {text:?}"
    );
}

/// A filtered payload has no computable length, so the terminator decides. This
/// is the fallback path, and it must still not leak.
#[test]
fn a_filtered_payload_falls_back_to_the_terminator() {
    let content = b"BT /F1 12 Tf 72 700 Td (Head) Tj ET\n\
        BI /W 2 /H 2 /CS /G /BPC 8 /F /AHx ID\n\
        48656c6c6f (not text) Tj\n\
        EI\n\
        BT /F1 12 Tf 72 680 Td (Tail) Tj ET";
    let text = text_of(&document(content));

    assert!(
        text.contains("Head") && text.contains("Tail"),
        "text on both sides must survive; got {text:?}"
    );
    assert!(
        !text.contains("not text") && !text.contains("48656c6c6f"),
        "an ASCII85/hex payload must not reach the text; got {text:?}"
    );
}
