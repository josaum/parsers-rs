//! Document parsing to the in-memory model and on to JSON.
//!
//! The input is built in memory and is the same on every run: 2,000 body
//! paragraphs of three formatted runs each, 100 tables, 200 footnotes, 80
//! comments and tracked insertions and deletions, deflated as a real document
//! is.

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::{Cursor, Write};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use docx_fast_core::{parse_docx_bytes, Block, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const PARAGRAPHS: usize = 2_000;
const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

const DOCUMENT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

const SENTENCE: &str = "The committee reviewed the proposal &amp; agreed that the schedule, \
    budget and staffing were adequate for the first phase of the work.";

fn styles_xml() -> String {
    let mut xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:styles xmlns:w="{W}">"#);
    for (id, name) in [
        ("Normal", "Normal"),
        ("Heading1", "heading 1"),
        ("Quote", "Quote"),
    ] {
        write!(
            xml,
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/></w:rPr></w:style>"#
        )
        .unwrap();
    }
    xml.push_str("</w:styles>");
    xml
}

struct Parts {
    document: String,
    footnotes: String,
    comments: String,
    footnote_count: usize,
    comment_count: usize,
    table_count: usize,
}

fn parts() -> Parts {
    let mut document =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="{W}"><w:body>"#);
    let mut footnotes = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:footnotes xmlns:w="{W}"><w:footnote w:id="-1" w:type="separator"><w:p><w:r><w:t>separator</w:t></w:r></w:p></w:footnote>"#
    );
    let mut comments =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:comments xmlns:w="{W}">"#);
    let (mut footnote_count, mut comment_count, mut table_count) = (0, 0, 0);

    for index in 0..PARAGRAPHS {
        let style = if index % 40 == 0 {
            "Heading1"
        } else {
            "Normal"
        };
        write!(
            document,
            r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t xml:space="preserve">Paragraph {index}. </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">{SENTENCE} </w:t></w:r><w:r><w:rPr><w:i/><w:u w:val="single"/></w:rPr><w:t>{SENTENCE}</w:t></w:r>"#
        )
        .unwrap();
        if index % 10 == 0 {
            footnote_count += 1;
            write!(
                document,
                r#"<w:r><w:footnoteReference w:id="{footnote_count}"/></w:r>"#
            )
            .unwrap();
            write!(
                footnotes,
                r#"<w:footnote w:id="{footnote_count}"><w:p><w:r><w:t>Note {footnote_count}: {SENTENCE}</w:t></w:r></w:p></w:footnote>"#
            )
            .unwrap();
        }
        if index % 25 == 0 {
            write!(
                document,
                r#"<w:commentRangeStart w:id="{comment_count}"/><w:r><w:t>commented</w:t></w:r><w:commentRangeEnd w:id="{comment_count}"/><w:r><w:commentReference w:id="{comment_count}"/></w:r>"#
            )
            .unwrap();
            write!(
                comments,
                r#"<w:comment w:id="{comment_count}" w:author="Reviewer {}" w:initials="R" w:date="2024-01-15T10:00:00Z"><w:p><w:r><w:t>Please check: {SENTENCE}</w:t></w:r></w:p></w:comment>"#,
                comment_count % 5
            )
            .unwrap();
            comment_count += 1;
        }
        if index % 15 == 0 {
            write!(
                document,
                r#"<w:ins w:id="{}" w:author="Editor" w:date="2024-02-01T09:00:00Z"><w:r><w:t xml:space="preserve"> inserted text</w:t></w:r></w:ins><w:del w:id="{}" w:author="Editor" w:date="2024-02-01T09:05:00Z"><w:r><w:delText xml:space="preserve"> deleted text</w:delText></w:r></w:del>"#,
                10_000 + index,
                20_000 + index
            )
            .unwrap();
        }
        document.push_str("</w:p>");
        if index % 20 == 0 {
            table_count += 1;
            document.push_str("<w:tbl>");
            for row in 0..4 {
                document.push_str("<w:tr>");
                for col in 0..4 {
                    write!(
                        document,
                        r#"<w:tc><w:p><w:r><w:t>T{table_count} r{row} c{col}</w:t></w:r></w:p></w:tc>"#
                    )
                    .unwrap();
                }
                document.push_str("</w:tr>");
            }
            document.push_str("</w:tbl>");
        }
    }
    document.push_str("</w:body></w:document>");
    footnotes.push_str("</w:footnotes>");
    comments.push_str("</w:comments>");
    Parts {
        document,
        footnotes,
        comments,
        footnote_count,
        comment_count,
        table_count,
    }
}

fn bench(c: &mut Criterion) {
    let parts = parts();
    let styles = styles_xml();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let zip_options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut inflated = 0;
    for (name, body) in [
        ("_rels/.rels", ROOT_RELS),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS),
        ("word/document.xml", parts.document.as_str()),
        ("word/footnotes.xml", parts.footnotes.as_str()),
        ("word/comments.xml", parts.comments.as_str()),
        ("word/styles.xml", styles.as_str()),
    ] {
        zip.start_file(name, zip_options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        inflated += body.len();
    }
    let bytes = zip.finish().unwrap().into_inner();
    let options = ParseOptions::default();

    // Peak memory is measured outside criterion: set this to write the
    // generated input to a file a one-shot driver can parse.
    if let Some(path) = std::env::var_os("PARSERS_BENCH_DUMP_INPUT") {
        std::fs::write(path, &bytes).unwrap();
    }

    // A package that parses to nothing would look very fast: check it first.
    let parsed = parse_docx_bytes(bytes.clone(), options).unwrap();
    let paragraphs = parsed
        .blocks
        .iter()
        .filter(|block| matches!(block, Block::Paragraph(_)))
        .count();
    let tables = parsed.blocks.len() - paragraphs;
    assert_eq!(paragraphs, PARAGRAPHS);
    assert_eq!(tables, parts.table_count);
    assert_eq!(
        parsed.footnotes.iter().filter(|note| note.id > 0).count(),
        parts.footnote_count
    );
    assert_eq!(parsed.comments.len(), parts.comment_count);
    assert!(parsed.revisions.len() >= PARAGRAPHS / 15);
    let json = serde_json::to_vec(&parsed).unwrap();
    drop(parsed);

    eprintln!(
        "docx input: {PARAGRAPHS} paragraphs, {tables} tables, {} footnotes, {} comments, {} bytes zipped, {inflated} bytes of XML, {} bytes of JSON",
        parts.footnote_count,
        parts.comment_count,
        bytes.len(),
        json.len()
    );

    let mut group = c.benchmark_group("docx_2000_paragraphs");
    group
        .throughput(Throughput::Bytes(inflated as u64))
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));
    group.bench_function("json", |b| {
        b.iter_batched(
            || bytes.clone(),
            |bytes| {
                let document = parse_docx_bytes(bytes, options).unwrap();
                black_box(serde_json::to_vec(&document).unwrap())
            },
            BatchSize::LargeInput,
        );
    });
    group.bench_function("parse_only", |b| {
        b.iter_batched(
            || bytes.clone(),
            |bytes| black_box(parse_docx_bytes(bytes, options).unwrap()),
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
