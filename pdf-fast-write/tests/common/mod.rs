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

/// How the rows of a cross-reference stream are written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    /// No filter.
    Raw,
    /// FlateDecode.
    Flate,
    /// FlateDecode over PNG `Up` rows (`/Predictor 12`).
    FlateUp,
}

/// Objects packed into one object stream.
pub struct Packed {
    /// The object stream's own number.
    pub stream: u32,
    /// `(object number, body)` in the order they are packed.
    pub objects: Vec<(u32, Vec<u8>)>,
    /// Whether the stream is FlateDecode-compressed.
    pub flate: bool,
}

/// Returns `bytes` compressed with zlib, as FlateDecode holds it.
pub fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).expect("compress");
    encoder.finish().expect("compress")
}

/// Returns an object stream object body holding `packed.objects`.
pub fn object_stream(packed: &Packed) -> Vec<u8> {
    let mut header = String::new();
    let mut objects = Vec::new();
    for (number, body) in &packed.objects {
        header.push_str(&format!("{number} {} ", objects.len()));
        objects.extend_from_slice(body);
        objects.push(b'\n');
    }
    let first = header.len();
    let mut decoded = header.into_bytes();
    decoded.extend_from_slice(&objects);
    let count = packed.objects.len();
    if packed.flate {
        stream(
            &format!("/Type /ObjStm /N {count} /First {first} /Filter /FlateDecode"),
            &deflate(&decoded),
        )
    } else {
        stream(
            &format!("/Type /ObjStm /N {count} /First {first}"),
            &decoded,
        )
    }
}

/// Where the rows of a cross-reference stream point.
#[derive(Clone, Copy, Debug)]
pub enum Row {
    /// A free entry.
    Free,
    /// An object at this byte offset, generation 0.
    At(usize),
    /// Object `index` of object stream `stream`.
    In(u32, u32),
}

/// Returns a cross-reference stream object body for `rows`, `/W [1 4 2]`.
///
/// `rows` are `(object number, row)` and are written as one subsection per
/// run of consecutive numbers; `dictionary` adds trailer entries.
pub fn xref_stream(rows: &[(u32, Row)], dictionary: &str, encoding: Rows) -> Vec<u8> {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|(number, _)| *number);
    let mut index: Vec<(u32, u32)> = Vec::new();
    let mut encoded = Vec::new();
    let mut previous = [0u8; 7];
    for (number, row) in &rows {
        match index.last_mut() {
            Some((first, count)) if *first + *count == *number => *count += 1,
            _ => index.push((*number, 1)),
        }
        let (kind, field2, field3) = match *row {
            Row::Free => (0u8, 0u32, 0xFFFFu16),
            Row::At(offset) => (1, u32::try_from(offset).expect("small"), 0),
            Row::In(stream, at) => (2, stream, u16::try_from(at).expect("small")),
        };
        let mut bytes = [0u8; 7];
        bytes[0] = kind;
        bytes[1..5].copy_from_slice(&field2.to_be_bytes());
        bytes[5..7].copy_from_slice(&field3.to_be_bytes());
        if encoding == Rows::FlateUp {
            encoded.push(2);
            for (byte, above) in bytes.iter().zip(previous) {
                encoded.push(byte.wrapping_sub(above));
            }
            previous = bytes;
        } else {
            encoded.extend_from_slice(&bytes);
        }
    }
    let index: Vec<String> = index
        .iter()
        .map(|(first, count)| format!("{first} {count}"))
        .collect();
    let size = rows.last().map_or(1, |(number, _)| number + 1);
    let head = format!(
        "/Type /XRef /W [1 4 2] /Index [{}] /Size {size} {dictionary}",
        index.join(" ")
    );
    match encoding {
        Rows::Raw => stream(&head, &encoded),
        Rows::Flate => stream(&format!("{head} /Filter /FlateDecode"), &deflate(&encoded)),
        Rows::FlateUp => stream(
            &format!("{head} /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 7 >>"),
            &deflate(&encoded),
        ),
    }
}

/// Appends `number 0 obj body endobj` to `pdf` and returns its offset.
pub fn append_object(pdf: &mut Vec<u8>, number: u32, body: &[u8]) -> usize {
    let at = pdf.len();
    pdf.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    at
}

/// Appends one revision whose cross-reference is a stream, numbered
/// `xref_number`, and returns the offset of that stream.
///
/// `plain` objects are written as they are, `packed` ones into object
/// streams; the section lists every one of them, the object streams and
/// itself. `dictionary` adds trailer entries (`/Root`, `/Prev`).
pub fn xref_stream_revision(
    pdf: &mut Vec<u8>,
    plain: &[(u32, Vec<u8>)],
    packed: &[Packed],
    xref_number: u32,
    dictionary: &str,
    encoding: Rows,
) -> usize {
    let mut rows = Vec::new();
    for (number, body) in plain {
        rows.push((*number, Row::At(append_object(pdf, *number, body))));
    }
    for stream in packed {
        let at = append_object(pdf, stream.stream, &object_stream(stream));
        rows.push((stream.stream, Row::At(at)));
        for (index, (number, _)) in stream.objects.iter().enumerate() {
            rows.push((*number, Row::In(stream.stream, index as u32)));
        }
    }
    let xref_at = pdf.len();
    rows.push((xref_number, Row::At(xref_at)));
    append_object(pdf, xref_number, &xref_stream(&rows, dictionary, encoding));
    pdf.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
    xref_at
}

/// Returns a PDF 1.5 document of `count` pages whose catalog, page tree,
/// pages and fonts live in one object stream and whose cross-reference is a
/// stream. Page `n` shows `Page n`. With [`Rows::Raw`] nothing in it is
/// compressed.
///
/// Object 1 is the catalog, 2 the page tree, 3 the font, then a page and
/// its content stream for each page; 5 is the object stream and 6 the
/// cross-reference stream.
pub fn compressed_document(count: u32, encoding: Rows) -> Vec<u8> {
    let (plain, packed) = compressed_objects(count, encoding != Rows::Raw);
    let mut pdf = b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n".to_vec();
    xref_stream_revision(&mut pdf, &plain, &[packed], 6, "/Root 1 0 R", encoding);
    pdf
}

/// Returns the content streams and the object stream of
/// [`compressed_document`].
pub fn compressed_objects(count: u32, flate: bool) -> (Vec<(u32, Vec<u8>)>, Packed) {
    let kids: Vec<String> = (0..count).map(|n| format!("{} 0 R", 10 + 2 * n)).collect();
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (
            2,
            format!(
                "<< /Type /Pages /Count {count} /Kids [{}] >>",
                kids.join(" ")
            )
            .into_bytes(),
        ),
        (3, FONT.as_bytes().to_vec()),
    ];
    let mut plain = Vec::new();
    for n in 0..count {
        objects.push((
            10 + 2 * n,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                11 + 2 * n
            )
            .into_bytes(),
        ));
        plain.push((11 + 2 * n, text_stream(&format!("Page {}", n + 1))));
    }
    let packed = Packed {
        stream: 5,
        objects,
        flate,
    };
    (plain, packed)
}
