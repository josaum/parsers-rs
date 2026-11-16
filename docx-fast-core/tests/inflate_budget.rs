//! `max_inflate` limits what the package expands to, counting each part
//! once however many readers need it.

use docx_fast_core::{parse_docx_bytes, Block, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

const LIMIT: usize = 1024 * 1024;

fn zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(bytes).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// A document part of exactly `size` bytes holding one long paragraph.
fn document(size: usize) -> Vec<u8> {
    let head = format!(r#"<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>"#);
    let tail = "</w:t></w:r></w:p></w:body></w:document>";
    let text = "x".repeat(size - head.len() - tail.len());
    format!("{head}{text}{tail}").into_bytes()
}

/// A document relationships part of exactly `size` bytes naming one image.
fn relationships(size: usize) -> Vec<u8> {
    let head = format!(r#"<Relationships xmlns="{PKG_REL}">"#);
    let tail = format!(
        r#"<Relationship Id="rId1" Type="{REL}/image" Target="media/image1.png"/></Relationships>"#
    );
    let padding = " ".repeat(size - head.len() - tail.len());
    format!("{head}{padding}{tail}").into_bytes()
}

fn options() -> ParseOptions {
    ParseOptions {
        max_inflate: LIMIT,
        ..ParseOptions::default()
    }
}

#[test]
fn a_document_part_just_under_the_limit_parses() {
    let bytes = zip(&[("word/document.xml", document(LIMIT - 1024))]);
    let doc = parse_docx_bytes(bytes, options()).expect("document parses");
    let Block::Paragraph(paragraph) = &doc.blocks[0] else {
        panic!("expected a paragraph");
    };
    assert!(paragraph.text.len() > LIMIT - 2048);
}

#[test]
fn the_document_relationships_are_counted_once() {
    // 600 KiB of document and 400 KiB of relationships: under the limit
    // together, over it if either is counted twice.
    let bytes = zip(&[
        ("word/document.xml", document(600 * 1024)),
        ("word/_rels/document.xml.rels", relationships(400 * 1024)),
    ]);
    let doc = parse_docx_bytes(bytes, options()).expect("document parses");
    assert_eq!(doc.images.len(), 1);
    assert_eq!(doc.images[0].target, "word/media/image1.png");
}

#[test]
fn a_document_part_over_the_limit_is_still_refused() {
    let bytes = zip(&[("word/document.xml", document(LIMIT + 1))]);
    let error = parse_docx_bytes(bytes, options()).expect_err("over the limit");
    assert!(format!("{error:#}").contains("max_inflate"), "{error:#}");
}
