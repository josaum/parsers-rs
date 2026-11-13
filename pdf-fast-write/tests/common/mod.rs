//! Builders for small classic-xref PDFs and a reader for what was extracted.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use pdf_fast_core::{ParseOptions, PdfDocument};

/// One revision of a document: objects, then a cross-reference section.
pub struct Revision {
    /// `(object number, body)` for each object written in this revision.
    pub objects: Vec<(u32, Vec<u8>)>,
    /// Object numbers this revision marks as free.
    pub freed: Vec<u32>,
    /// Entries of the trailer dictionary besides `/Size` and `/Prev`.
    pub trailer: String,
}

/// Returns a revision with `objects` and a trailer naming `root`.
pub fn revision(objects: Vec<(u32, Vec<u8>)>, root: u32) -> Revision {
    Revision {
        objects,
        freed: Vec::new(),
        trailer: format!("/Root {root} 0 R"),
    }
}

/// Returns `body` as a stream object with the right `/Length`.
pub fn stream(dictionary: &str, body: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dictionary} /Length {} >>\nstream\n", body.len()).into_bytes();
    object.extend_from_slice(body);
    object.extend_from_slice(b"\nendstream");
    object
}

/// Returns a content stream that shows `text`.
pub fn text_stream(text: &str) -> Vec<u8> {
    stream(
        "",
        format!("BT /F1 12 Tf 100 700 Td ({text}) Tj ET").as_bytes(),
    )
}

/// Writes the revisions one after another, each as an incremental update of
/// the ones before it, the way an editor saves a file several times.
pub fn build(revisions: &[Revision]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut previous: Option<usize> = None;
    let mut size = 1;
    for revision in revisions {
        let mut entries: Vec<(u32, String)> = Vec::new();
        for (number, body) in &revision.objects {
            entries.push((*number, format!("{:010} 00000 n \n", pdf.len())));
            pdf.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
            size = size.max(number + 1);
        }
        for number in &revision.freed {
            entries.push((*number, "0000000000 00001 f \n".to_string()));
        }
        entries.sort();
        let xref_at = pdf.len();
        pdf.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        for (number, entry) in entries {
            pdf.extend_from_slice(format!("{number} 1\n{entry}").as_bytes());
        }
        let prev = previous.map_or(String::new(), |offset| format!(" /Prev {offset}"));
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {size} {}{prev} >>\nstartxref\n{xref_at}\n%%EOF\n",
                revision.trailer
            )
            .as_bytes(),
        );
        previous = Some(xref_at);
    }
    pdf
}

/// The font dictionary every test page uses.
pub const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// Returns a page dictionary under `parent` showing content stream `contents`.
pub fn page(parent: u32, contents: u32, extra: &str) -> Vec<u8> {
    format!(
        "<< /Type /Page /Parent {parent} 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 {FONT} >> >> /Contents {contents} 0 R {extra} >>"
    )
    .into_bytes()
}

/// Returns a document of `count` pages; page `n` shows the text `Page n`.
///
/// Object 1 is the catalog, 2 the page tree, then a page and its content
/// stream for each page.
pub fn simple_document(count: u32) -> Vec<u8> {
    let mut objects = vec![(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec())];
    let kids: Vec<String> = (0..count).map(|n| format!("{} 0 R", 3 + 2 * n)).collect();
    objects.push((
        2,
        format!(
            "<< /Type /Pages /Count {count} /Kids [{}] >>",
            kids.join(" ")
        )
        .into_bytes(),
    ));
    for n in 0..count {
        objects.push((3 + 2 * n, page(2, 4 + 2 * n, "")));
        objects.push((4 + 2 * n, text_stream(&format!("Page {}", n + 1))));
    }
    build(&[revision(objects, 1)])
}

/// Opens `pdf` with pdf-fast-core and returns the text shown on each page.
pub fn page_texts(pdf: &[u8]) -> Vec<String> {
    let document =
        PdfDocument::from_slice(pdf, ParseOptions::default()).expect("output opens in the reader");
    let mut texts = vec![String::new(); document.pages().len()];
    for run in document.semantic_text_runs() {
        texts[run.page_index].push_str(&run.text);
    }
    texts
}
