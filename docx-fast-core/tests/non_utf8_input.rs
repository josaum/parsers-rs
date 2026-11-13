//! Parts that are not UTF-8 read the way they always have.
//!
//! A stray byte that is not UTF-8 (here 0xE9, "é" in ISO-8859-1) costs that
//! character, read as U+FFFD, wherever docx reads it, and nothing where docx
//! does not read it. A UTF-16 part is transcoded. These outputs are what the
//! parser produced on quick-xml 0.41, pinned so the 0.42 reader, which
//! rejects such bytes itself, cannot change them.

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, bytes) in parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(bytes).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// `body` (UTF-8 text with `{E9}` marking each stray byte) as part bytes.
fn latin1_part(body: &str) -> Vec<u8> {
    let xml = format!(r#"<?xml version="1.0" encoding="ISO-8859-1"?>{body}"#);
    let mut bytes = Vec::new();
    for (index, piece) in xml.split("{E9}").enumerate() {
        if index > 0 {
            bytes.push(0xE9);
        }
        bytes.extend_from_slice(piece.as_bytes());
    }
    bytes
}

fn package(document: Vec<u8>, method: CompressionMethod) -> Vec<u8> {
    let parts = [
        (
            "_rels/.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        ("word/document.xml", document),
        (
            "word/_rels/document.xml.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/><Relationship Id="rId5" Type="{REL}/comments" Target="comments.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        (
            "word/styles.xml",
            latin1_part(&format!(
                r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:styleId="Caf{{E9}}"><w:name w:val="Caf{{E9}} style"/></w:style></w:styles>"#
            )),
        ),
        (
            "word/comments.xml",
            latin1_part(&format!(
                r#"<w:comments xmlns:w="{W}"><w:comment w:id="1" w:author="Zo{{E9}}" w:initials="Z{{E9}}"><w:p><w:r><w:t>note {{E9}}</w:t></w:r></w:p></w:comment></w:comments>"#
            )),
        ),
        (
            "docProps/core.xml",
            latin1_part(
                r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Caf{E9}</dc:title></cp:coreProperties>"#,
            ),
        ),
    ];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
    for (name, bytes) in &parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(bytes).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

fn parse(bytes: Vec<u8>) -> DocxDocument {
    parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses")
}

fn first_paragraph_text(doc: &DocxDocument) -> &str {
    let Block::Paragraph(paragraph) = &doc.blocks[0] else {
        panic!("expected a paragraph");
    };
    &paragraph.text
}

#[test]
fn stray_latin1_bytes_cost_one_character_each() {
    // Stray bytes in run text and in attributes docx reads, and in places it
    // does not read: an XML comment, an unread attribute, an unread element.
    let document = latin1_part(&format!(
        r#"<w:document xmlns:w="{W}" xmlns:r="{REL}"><!-- caf{{E9}} --><w:body><w:p w:rsidR="{{E9}}"><w:pPr><w:pStyle w:val="Caf{{E9}}"/></w:pPr><w:r><w:t>caf{{E9}} au lait</w:t></w:r><w:r><w:unknown>{{E9}}</w:unknown></w:r></w:p></w:body></w:document>"#
    ));
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        let doc = parse(package(document.clone(), method));
        assert_eq!(
            first_paragraph_text(&doc),
            "caf\u{FFFD} au lait",
            "{method:?}"
        );
        let Block::Paragraph(paragraph) = &doc.blocks[0] else {
            panic!("expected a paragraph");
        };
        assert_eq!(paragraph.style_id.as_deref(), Some("Caf\u{FFFD}"));
        assert_eq!(paragraph.style_name.as_deref(), Some("Caf\u{FFFD} style"));
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].author.as_deref(), Some("Zo\u{FFFD}"));
        assert_eq!(doc.comments[0].initials.as_deref(), Some("Z\u{FFFD}"));
        assert_eq!(
            doc.metadata.core.get("title").map(String::as_str),
            Some("Caf\u{FFFD}")
        );
    }
}

#[test]
fn utf16le_part_is_transcoded() {
    let xml = format!(
        "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-16\"?><w:document xmlns:w=\"{W}\"><w:body><w:p><w:r><w:t>caf\u{e9} \u{65E5}\u{672C}</w:t></w:r></w:p></w:body></w:document>"
    );
    let document: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let doc = parse(zip(&[("word/document.xml", document)]));
    assert_eq!(first_paragraph_text(&doc), "caf\u{e9} \u{65E5}\u{672C}");
}
