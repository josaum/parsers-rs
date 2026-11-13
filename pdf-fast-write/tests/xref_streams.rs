//! Sources whose cross-reference is a stream (PDF 1.5+) and whose objects
//! live in object streams: the pages are extracted as from a classic file,
//! the output keeps a classic table, and a malformed stream is refused for
//! its own reason, never looped on.

mod common;

use common::{
    append_object, compressed_document, compressed_objects, deflate, object_stream, page_texts,
    stream, text_stream, xref_stream, xref_stream_revision, Packed, Row, Rows, FONT,
};
use pdf_fast_write::PdfWriter;

fn extract(pdf: &[u8], pages: &[usize]) -> Result<Vec<u8>, String> {
    PdfWriter::extract_pages_from_bytes(pdf, pages).map_err(|error| format!("{error:#}"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Returns the offset `startxref` names.
fn startxref(pdf: &[u8]) -> usize {
    let text = String::from_utf8_lossy(pdf);
    let at = text.rfind("startxref").expect("startxref");
    text[at + 9..]
        .split_whitespace()
        .next()
        .and_then(|digits| digits.parse().ok())
        .expect("offset")
}

/// Rewrites the dictionary and data of the cross-reference stream that
/// `startxref` names, which [`common::xref_stream_revision`] writes last.
fn rewrite_xref_stream(pdf: &[u8], edit: impl Fn(String, Vec<u8>) -> (String, Vec<u8>)) -> Vec<u8> {
    let at = startxref(pdf);
    let object = &pdf[at..];
    let open = object.windows(2).position(|w| w == b"<<").expect("<<");
    let header = String::from_utf8_lossy(&object[..open]).to_string();
    let close = object
        .windows(b">>\nstream\n".len())
        .position(|w| w == b">>\nstream\n")
        .expect("stream");
    let dictionary = String::from_utf8_lossy(&object[open + 2..close]).to_string();
    let length_at = dictionary.rfind("/Length").expect("/Length");
    let length: usize = dictionary[length_at + 7..].trim().parse().expect("length");
    let data_start = close + b">>\nstream\n".len();
    let data = object[data_start..data_start + length].to_vec();
    let (dictionary, data) = edit(dictionary[..length_at].to_string(), data);
    let mut out = pdf[..at].to_vec();
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&stream(&dictionary, &data));
    out.extend_from_slice(format!("\nendobj\nstartxref\n{at}\n%%EOF\n").as_bytes());
    out
}

/// WHY: PDF 1.5+ writers put the catalog, the page tree, the pages and their
/// fonts in object streams and index them with a cross-reference stream.
/// Such a source must extract like a classic one, under every row encoding.
#[test]
fn a_document_with_object_streams_is_extracted() {
    for encoding in [Rows::Raw, Rows::Flate, Rows::FlateUp] {
        let pdf = compressed_document(3, encoding);
        let texts = |pages: &[usize]| page_texts(&extract(&pdf, pages).expect("extracts"));
        assert_eq!(texts(&[2, 1]), ["Page 2", "Page 1"]);
        assert_eq!(texts(&[1, 2, 3]), ["Page 1", "Page 2", "Page 3"]);
        let output = extract(&pdf, &[3]).expect("extracts");
        assert!(output.starts_with(b"%PDF-1.4"));
        assert!(
            contains(&output, b"\nxref\n"),
            "the output keeps a classic table"
        );
        for marker in [&b"/XRef"[..], b"/ObjStm", b"/Prev"] {
            assert!(!contains(&output, marker), "{marker:?} in the output");
        }
    }
}

/// WHY: a hybrid file keeps a classic table for old readers and places the
/// objects in object streams through `/XRefStm`. The table marks those
/// objects free; the stream's entries for them must still be read.
#[test]
fn a_hybrid_document_reads_the_objects_its_stream_places() {
    let (plain, packed) = compressed_objects(2, true);
    let mut pdf = b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut table: Vec<(u32, String)> = Vec::new();
    for (number, body) in &plain {
        let at = append_object(&mut pdf, *number, body);
        table.push((*number, format!("{at:010} 00000 n \n")));
    }
    let at = append_object(&mut pdf, packed.stream, &object_stream(&packed));
    table.push((packed.stream, format!("{at:010} 00000 n \n")));
    let mut rows = Vec::new();
    for (index, (number, _)) in packed.objects.iter().enumerate() {
        table.push((*number, "0000000000 00001 f \n".to_string()));
        rows.push((*number, Row::In(packed.stream, index as u32)));
    }
    let hidden_at = append_object(&mut pdf, 98, &xref_stream(&rows, "", Rows::Flate));
    table.push((98, format!("{hidden_at:010} 00000 n \n")));
    table.sort();
    let xref_at = pdf.len();
    pdf.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (number, entry) in table {
        pdf.extend_from_slice(format!("{number} 1\n{entry}").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size 99 /Root 1 0 R /XRefStm {hidden_at} >>\nstartxref\n{xref_at}\n%%EOF\n"
        )
        .as_bytes(),
    );

    let output = extract(&pdf, &[2, 1]).expect("extracts");
    assert_eq!(page_texts(&output), ["Page 2", "Page 1"]);
    assert!(!contains(&output, b"/XRef"));
}

/// WHY: an editor saving a PDF 1.5 file again appends a cross-reference
/// stream with `/Prev`. The newest section wins, including the entries it
/// marks free, and it may describe only a few objects (`/Index` not at 0).
#[test]
fn a_prev_chain_of_cross_reference_streams_reads_the_newest_revision() {
    let mut pdf = compressed_document(2, Rows::Flate);
    let previous = startxref(&pdf);
    let revised = append_object(&mut pdf, 11, &text_stream("Page 1, revised"));
    let xref_at = pdf.len();
    let rows = [
        (11, Row::At(revised)),
        (13, Row::Free),
        (98, Row::At(xref_at)),
    ];
    append_object(
        &mut pdf,
        98,
        &xref_stream(
            &rows,
            &format!("/Root 1 0 R /Prev {previous}"),
            Rows::FlateUp,
        ),
    );
    pdf.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());

    let output = extract(&pdf, &[1]).expect("extracts");
    assert_eq!(page_texts(&output), ["Page 1, revised"]);
    let error = extract(&pdf, &[2]).expect_err("page 2's content was deleted");
    assert!(error.contains("object 13 is absent from xref"), "{error}");
}

/// WHY: in a PDF 1.5 file the `/Encrypt` key is in the cross-reference
/// stream's dictionary; its object streams are ciphertext.
#[test]
fn an_encrypted_document_with_a_cross_reference_stream_is_refused() {
    let (plain, packed) = compressed_objects(1, true);
    let mut pdf = b"%PDF-1.5\n".to_vec();
    xref_stream_revision(
        &mut pdf,
        &plain,
        &[packed],
        99,
        "/Root 1 0 R /Encrypt << /Filter /Standard >>",
        Rows::Flate,
    );
    let error = extract(&pdf, &[1]).expect_err("encrypted");
    assert!(error.contains("encrypted"), "{error}");
}

/// WHY: the cross-reference stream and the object streams are the source's
/// structure. A page that names one as its content must not carry it, with
/// every offset and object of the source, into the output.
#[test]
fn cross_reference_and_object_streams_are_never_copied() {
    for contents in [90, 99] {
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] /Resources << /Font << /F1 {FONT} >> >> /Contents {contents} 0 R >>"
        );
        let packed = Packed {
            stream: 90,
            objects: vec![
                (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
                (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
                (3, page.into_bytes()),
            ],
            flate: true,
        };
        let mut pdf = b"%PDF-1.5\n".to_vec();
        xref_stream_revision(&mut pdf, &[], &[packed], 99, "/Root 1 0 R", Rows::Flate);
        let error = extract(&pdf, &[1]).expect_err("structure is not content");
        assert!(error.contains("is not page content"), "{contents}: {error}");
    }
}

/// Builds a one-page document whose object stream and rows are given.
fn with_rows(objstm: &[u8], rows_for: impl Fn(usize, usize) -> Vec<(u32, Row)>) -> Vec<u8> {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    let content = append_object(&mut pdf, 11, &text_stream("x"));
    let container = append_object(&mut pdf, 90, objstm);
    let xref_at = pdf.len();
    let mut rows = rows_for(content, container);
    rows.push((99, Row::At(xref_at)));
    append_object(&mut pdf, 99, &xref_stream(&rows, "/Root 1 0 R", Rows::Raw));
    pdf.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
    pdf
}

/// The rows of a well-formed [`with_rows`] document with three objects in
/// object stream 90.
fn plain_rows(content: usize, container: usize) -> Vec<(u32, Row)> {
    vec![
        (1, Row::In(90, 0)),
        (2, Row::In(90, 1)),
        (3, Row::In(90, 2)),
        (11, Row::At(content)),
        (90, Row::At(container)),
    ]
}

/// The objects of a [`with_rows`] document, as an object stream would
/// hold them.
fn three_objects() -> Packed {
    Packed {
        stream: 90,
        objects: vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (
                3,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] /Resources << /Font << /F1 {FONT} >> >> /Contents 11 0 R >>"
                )
                .into_bytes(),
            ),
        ],
        flate: false,
    }
}

/// WHY: every field of a cross-reference or object stream comes from the
/// file. Each malformed one must be refused for its own reason: never a
/// panic, a loop, or an allocation sized from a count the file only claims.
#[test]
fn malformed_streams_are_refused_with_their_reason() {
    let good = with_rows(&object_stream(&three_objects()), plain_rows);
    assert_eq!(
        page_texts(&extract(&good, &[1]).expect("the twin extracts")),
        ["x"]
    );
    // Eleven rows of eight bytes: not a whole number of six-byte rows.
    let flate_up = compressed_document(3, Rows::FlateUp);
    let flate = compressed_document(2, Rows::Flate);

    let replace = |pdf: &[u8], from: &str, to: &str| {
        rewrite_xref_stream(pdf, |dictionary, data| (dictionary.replace(from, to), data))
    };
    let mut cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("wide /W", replace(&good, "/W [1 4 2]", "/W [1 9 2]"), "/W"),
        ("empty /W", replace(&good, "/W [1 4 2]", "/W [0 0 0]"), "/W"),
        ("short /W", replace(&good, "/W [1 4 2]", "/W [1 4]"), "/W"),
        (
            "huge /Index",
            rewrite_xref_stream(&good, |dictionary, data| {
                let start = dictionary.find("/Index [").expect("index");
                let end = start + dictionary[start..].find(']').expect("]");
                let edited = format!(
                    "{}/Index [0 4000000000]{}",
                    &dictionary[..start],
                    &dictionary[end + 1..]
                );
                (edited, data)
            }),
            "/Index",
        ),
        (
            "truncated stream",
            rewrite_xref_stream(&flate, |dictionary, data| {
                (dictionary, data[..data.len() / 2].to_vec())
            }),
            "cross-reference stream",
        ),
        (
            "predictor with wrong columns",
            replace(&flate_up, "/Columns 7", "/Columns 5"),
            "predictor",
        ),
        (
            "/Prev to itself",
            replace(
                &good,
                "/Root 1 0 R",
                &format!("/Root 1 0 R /Prev {}", startxref(&good)),
            ),
            "cyclic",
        ),
    ];

    // An object stream whose /N claims more objects than its header lists.
    let objects = three_objects();
    let honest = object_stream(&objects);
    let claims_more = String::from_utf8_lossy(&honest).replace("/N 3", "/N 9");
    cases.push((
        "/N larger than the content",
        with_rows(claims_more.as_bytes(), plain_rows),
        "/N",
    ));
    // An object stream the cross-reference places inside itself.
    cases.push((
        "object stream inside itself",
        with_rows(&honest, |content, _| {
            vec![
                (1, Row::In(90, 0)),
                (2, Row::In(90, 1)),
                (3, Row::In(90, 2)),
                (11, Row::At(content)),
                (90, Row::In(90, 3)),
            ]
        }),
        "itself in an object stream",
    ));
    // Two objects at one offset.
    let overlapping = stream(
        "/Type /ObjStm /N 3 /First 12",
        b"1 0 2 0 3 0 << /Type /Catalog /Pages 2 0 R >>",
    );
    cases.push((
        "overlapping objects",
        with_rows(&overlapping, plain_rows),
        "ascending",
    ));
    // A /First past the end of the decoded data.
    let first_past_end = String::from_utf8_lossy(&honest).replace("/First ", "/First 9999");
    cases.push((
        "/First past the data",
        with_rows(first_past_end.as_bytes(), plain_rows),
        "/First",
    ));
    // A deflate stream cut short inside an object stream.
    let packed = deflate(b"1 0 2 40 << /Type /Catalog /Pages 2 0 R >> << /Type /Pages >>");
    let cut = stream(
        "/Type /ObjStm /N 2 /First 9 /Filter /FlateDecode",
        &packed[..packed.len() / 2],
    );
    cases.push((
        "truncated object stream",
        with_rows(&cut, plain_rows),
        "object stream 90",
    ));

    for (name, pdf, reason) in cases {
        let error = extract(&pdf, &[1]).expect_err(name);
        assert!(error.contains(reason), "{name}: {error}");
    }
}
