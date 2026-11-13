//! The source PDF is untrusted: a malformed one must be an error, never a
//! panic, a stack overflow, an unbounded loop or an allocation out of
//! proportion to the file. Whatever is returned must open in pdf-fast-core.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

mod common;

use common::{
    build, compressed_document, page, revision, simple_document, stream, text_stream, Revision,
    Rows,
};
use pdf_fast_core::{ParseOptions, PdfDocument};
use pdf_fast_write::PdfWriter;

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

/// A source that reaches every branch of the subsetter: inherited page
/// attributes under a nested page tree, an indirect stream length, an
/// annotation, binary stream bytes, and three saved revisions.
fn revised_document() -> Vec<u8> {
    let first = revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R /Metadata 12 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Count 2 /Kids [7 0 R] /MediaBox [0 0 612 792] /Rotate 90 \
                  /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>"
                    .to_vec(),
            ),
            (7, b"<< /Type /Pages /Parent 2 0 R /Count 2 /Kids [3 0 R 5 0 R] /CropBox [0 0 600 700] >>".to_vec()),
            (3, b"<< /Type /Page /Parent 7 0 R /Contents 4 0 R /Annots [9 0 R] >>".to_vec()),
            (4, b"<< /Length 8 0 R >>\nstream\nBT (\xff\x00 one) Tj ET % endstream\r\nendstream".to_vec()),
            (8, b"29".to_vec()),
            (5, b"<< /Type /Page /Parent 7 0 R /Contents [6 0 R 6 0 R] >>".to_vec()),
            (6, text_stream("two (nested \\) parens)")),
            (9, b"<< /Type /Annot /Subtype /Text /Rect [1 2 3 4] /Contents (note % not a comment) /P 3 0 R >>".to_vec()),
            (12, stream("/Type /Metadata", b"<xmp/>")),
        ],
        1,
    );
    let second = Revision {
        objects: vec![
            (6, text_stream("two, revised")),
            (10, b"<< /Producer (x) >>".to_vec()),
        ],
        freed: vec![12],
        trailer: "/Root 1 0 R /Info 10 0 R".to_string(),
    };
    let third = revision(
        vec![(
            9,
            b"<< /Type /Annot /Subtype /Text /Rect [1 2 3 4] >>".to_vec(),
        )],
        1,
    );
    build(&[first, second, third])
}

const SELECTIONS: [&[usize]; 3] = [&[1], &[2, 1, 1], &[1, 2, 3]];

/// Returns how many pages pdf-fast-core finds in `pdf`, if it opens.
fn reader_page_count(pdf: &[u8]) -> Option<usize> {
    PdfDocument::from_slice(pdf, ParseOptions::default())
        .ok()
        .map(|document| document.pages().len())
}

/// Extracts each selection and checks any output against the reader.
///
/// Objects are copied as they are, so a page the mutation damaged is as
/// unreadable in the output as in the source and the reader may count fewer
/// pages than were selected; it must never count more. The exact count and
/// the text are asserted on undamaged sources in `tests/hardening.rs`.
fn extract_and_read_back(source: &[u8]) {
    for pages in SELECTIONS {
        let Ok(output) = PdfWriter::extract_pages_from_bytes(source, pages) else {
            continue;
        };
        let read =
            reader_page_count(&output).expect("an extracted document opens in pdf-fast-core");
        assert!(
            read <= pages.len(),
            "{read} pages from a selection of {pages:?}"
        );
    }
}

#[test]
fn the_seed_documents_are_extracted() {
    for seed in [
        revised_document(),
        simple_document(3),
        compressed_document(3, Rows::Raw),
        compressed_document(3, Rows::FlateUp),
    ] {
        let output = PdfWriter::extract_pages_from_bytes(&seed, &[2, 1, 1])
            .unwrap_or_else(|error| panic!("seed extracts: {error:#}"));
        let document = PdfDocument::from_slice(&output, ParseOptions::default()).expect("opens");
        assert_eq!(document.pages().len(), 3);
    }
}

#[test]
fn mutated_documents_never_panic_and_always_read_back() {
    // The unfiltered compressed seed lets mutations reach the parsing of
    // cross-reference and object streams instead of dying in inflate.
    let seeds = [
        revised_document(),
        simple_document(3),
        compressed_document(3, Rows::Raw),
        compressed_document(3, Rows::FlateUp),
    ];
    smoke::assert_no_panics(
        "pdf-fast-write source",
        &seeds,
        20_000,
        extract_and_read_back,
    );
}

/// Structures random mutation almost never produces: deep nesting, long
/// chains, cycles and counts far larger than the file.
#[test]
fn hostile_structures_are_bounded_on_a_small_stack() {
    let document = |page_extra: Vec<u8>, extra: Vec<(u32, Vec<u8>)>| {
        let mut body =
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] /Contents 4 0 R /X ".to_vec();
        body.extend_from_slice(&page_extra);
        body.extend_from_slice(b" >>");
        let mut objects = vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, body),
            (4, text_stream("x")),
        ];
        objects.extend(extra);
        build(&[revision(objects, 1)])
    };
    const DEPTH: usize = 100_000;
    let chain: Vec<(u32, Vec<u8>)> = (0..50_000u32)
        .map(|link| {
            (
                link + 10,
                format!("[{} 0 R]", (link + 1) % 50_000 + 10).into_bytes(),
            )
        })
        .collect();
    let mut kids_cycle = simple_document(1);
    let at = String::from_utf8_lossy(&kids_cycle)
        .find("[3 0 R]")
        .expect("kids");
    kids_cycle[at..at + 7].copy_from_slice(b"[2 0 R]");
    let mut huge_subsection = simple_document(2);
    let at = String::from_utf8_lossy(&huge_subsection)
        .find("xref\n0 1\n")
        .expect("xref");
    huge_subsection.splice(at + 5..at + 8, b"4294967295 4294967295".iter().copied());

    let cases: Vec<Vec<u8>> = vec![
        document(smoke::nested(b"[", b"1", b"]", DEPTH), Vec::new()),
        document(smoke::nested(b"<<", b"", b">>", DEPTH), Vec::new()),
        document(smoke::nested(b"<< /K ", b"1", b" >>", DEPTH), Vec::new()),
        document(smoke::nested(b"(", b"x", b")", DEPTH), Vec::new()),
        document(smoke::nested(b"(", b"x", b"", DEPTH), Vec::new()),
        document(b"10 0 R".to_vec(), chain),
        document(b"3 0 R".to_vec(), Vec::new()),
        document(b"1 0 R ".repeat(DEPTH), Vec::new()),
        kids_cycle,
        huge_subsection,
        page(2, 4, "").repeat(1_000),
        String::from_utf8_lossy(&compressed_document(2, Rows::Raw))
            .replace("/Index [", "/Index [0 4294967295 ")
            .into_bytes(),
        String::from_utf8_lossy(&compressed_document(2, Rows::Raw))
            .replace("/W [1 4 2]", "/W [8 8 8]")
            .into_bytes(),
    ];
    for case in cases {
        smoke::run_on_small_stack(move || extract_and_read_back(&case));
    }
}
