//! Presentation parsing to the in-memory model and on to JSON.
//!
//! The input is built in memory and is the same on every run: 200 slides,
//! each with a title, a five-paragraph body, a 4x4 table and a notes slide,
//! deflated as a real deck is.

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::{Cursor, Write};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use pptx_fast_core::{parse_pptx_bytes, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const SLIDES: usize = 200;
const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

const SENTENCE: &str = "Revenue grew in every region &amp; margins held despite higher \
    input costs across the quarter.";

fn slide_xml(index: usize) -> String {
    let mut xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld name="Slide {index}"><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/></p:nvSpPr><p:txBody><a:p><a:r><a:rPr lang="en-US" b="1"/><a:t>Quarterly review, part {index}</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content 2"/></p:nvSpPr><p:txBody>"#
    );
    for paragraph in 0..5 {
        write!(
            xml,
            r#"<a:p><a:pPr lvl="{}"/><a:r><a:rPr lang="en-US"/><a:t>Point {paragraph}: </a:t></a:r><a:r><a:rPr lang="en-US" i="1"/><a:t>{SENTENCE}</a:t></a:r></a:p>"#,
            paragraph % 2
        )
        .unwrap();
    }
    xml.push_str(r#"</p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData><a:tbl>"#);
    for row in 0..4 {
        xml.push_str("<a:tr>");
        for col in 0..4 {
            write!(
                xml,
                r#"<a:tc><a:txBody><a:p><a:r><a:t>S{index} r{row} c{col}</a:t></a:r></a:p></a:txBody></a:tc>"#
            )
            .unwrap();
        }
        xml.push_str("</a:tr>");
    }
    xml.push_str(
        "</a:tbl></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>",
    );
    xml
}

fn notes_xml(index: usize) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><p:notes xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder 2"/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Speaker notes for slide {index}. {SENTENCE}</a:t></a:r></a:p><a:p><a:r><a:t>{SENTENCE}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
    )
}

/// The generated deck and the total size of its parts before compression.
fn deck() -> (Vec<u8>, usize) {
    let mut presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst>"#
    );
    let mut presentation_rels =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS}">"#);
    let mut files: Vec<(String, String)> = Vec::new();
    for index in 1..=SLIDES {
        write!(
            presentation,
            r#"<p:sldId id="{}" r:id="rId{index}"/>"#,
            255 + index
        )
        .unwrap();
        write!(
            presentation_rels,
            r#"<Relationship Id="rId{index}" Type="{R}/slide" Target="slides/slide{index}.xml"/>"#
        )
        .unwrap();
        files.push((format!("ppt/slides/slide{index}.xml"), slide_xml(index)));
        files.push((
            format!("ppt/slides/_rels/slide{index}.xml.rels"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="{R}/notesSlide" Target="../notesSlides/notesSlide{index}.xml"/></Relationships>"#
            ),
        ));
        files.push((
            format!("ppt/notesSlides/notesSlide{index}.xml"),
            notes_xml(index),
        ));
    }
    presentation.push_str("</p:sldIdLst></p:presentation>");
    presentation_rels.push_str("</Relationships>");
    files.push((
        "_rels/.rels".to_string(),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="{R}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
        ),
    ));
    files.push(("ppt/presentation.xml".to_string(), presentation));
    files.push((
        "ppt/_rels/presentation.xml.rels".to_string(),
        presentation_rels,
    ));

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut inflated = 0;
    for (name, body) in &files {
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        inflated += body.len();
    }
    (zip.finish().unwrap().into_inner(), inflated)
}

fn bench(c: &mut Criterion) {
    let (bytes, inflated) = deck();
    let options = ParseOptions::default();

    // A package that parses to nothing would look very fast: check it first.
    let parsed = parse_pptx_bytes(bytes.clone(), options).unwrap();
    assert_eq!(parsed.slides.len(), SLIDES);
    for slide in &parsed.slides {
        assert_eq!(slide.shapes.len(), 2);
        assert_eq!(slide.tables.len(), 1);
        assert!(!slide.notes.is_empty());
    }
    let json = serde_json::to_vec(&parsed).unwrap();
    drop(parsed);

    eprintln!(
        "pptx input: {SLIDES} slides, {} bytes zipped, {inflated} bytes of XML, {} bytes of JSON",
        bytes.len(),
        json.len()
    );

    let mut group = c.benchmark_group("pptx_200_slides");
    group
        .throughput(Throughput::Bytes(inflated as u64))
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));
    group.bench_function("json", |b| {
        b.iter_batched(
            || bytes.clone(),
            |bytes| {
                let document = parse_pptx_bytes(bytes, options).unwrap();
                black_box(serde_json::to_vec(&document).unwrap())
            },
            BatchSize::LargeInput,
        );
    });
    group.bench_function("parse_only", |b| {
        b.iter_batched(
            || bytes.clone(),
            |bytes| black_box(parse_pptx_bytes(bytes, options).unwrap()),
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
