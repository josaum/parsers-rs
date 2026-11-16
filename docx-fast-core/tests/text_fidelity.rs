//! Document properties share the body's text rules: one bad byte costs one
//! character, and CDATA is literal text.

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const DOCUMENT: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Body text</w:t></w:r></w:p></w:body></w:document>"#;

const CORE_NS: &str = r#"xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/""#;
const APP_NS: &str =
    r#"xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties""#;
const CUSTOM_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes""#;

fn parse(core: &[u8], app: &[u8], custom: &[u8]) -> DocxDocument {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in [
        ("word/document.xml", DOCUMENT),
        ("docProps/core.xml", core),
        ("docProps/app.xml", app),
        ("docProps/custom.xml", custom),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses")
}

/// Replaces every `@` with the byte 0xE9, which is not valid UTF-8 on its own.
fn with_bad_byte(xml: String) -> Vec<u8> {
    xml.bytes()
        .map(|byte| if byte == b'@' { 0xE9 } else { byte })
        .collect()
}

fn body_text(doc: &DocxDocument) -> &str {
    match &doc.blocks[0] {
        Block::Paragraph(paragraph) => &paragraph.text,
        other => panic!("expected a paragraph, got {other:?}"),
    }
}

#[test]
fn one_invalid_byte_in_document_properties_costs_one_character() {
    let doc = parse(
        &with_bad_byte(format!(
            "<cp:coreProperties {CORE_NS}><dc:title>T@tle &amp; more</dc:title><dc:creator>Ada</dc:creator></cp:coreProperties>"
        )),
        &with_bad_byte(format!(
            "<Properties {APP_NS}><Company>Acm@ &lt;Ltd&gt;</Company><Pages>3</Pages></Properties>"
        )),
        &with_bad_byte(format!(
            r#"<Properties {CUSTOM_NS}><property fmtid="x" pid="2" name="Client"><vt:lpwstr>Cl@ent &amp; Co</vt:lpwstr></property></Properties>"#
        )),
    );
    assert_eq!(body_text(&doc), "Body text");
    assert_eq!(
        doc.metadata.core.get("title").map(String::as_str),
        Some("T\u{fffd}tle & more")
    );
    assert_eq!(
        doc.metadata.core.get("creator").map(String::as_str),
        Some("Ada")
    );
    assert_eq!(
        doc.metadata.app.get("Company").map(String::as_str),
        Some("Acm\u{fffd} <Ltd>")
    );
    assert_eq!(doc.page_count, Some(3));
    assert_eq!(
        doc.metadata.custom.get("Client").map(String::as_str),
        Some("Cl\u{fffd}ent & Co")
    );
}

#[test]
fn an_unknown_entity_in_document_properties_is_kept_as_written() {
    let doc = parse(
        format!("<cp:coreProperties {CORE_NS}><dc:title>a &bogus; b</dc:title></cp:coreProperties>")
            .as_bytes(),
        format!("<Properties {APP_NS}><Pages>3</Pages></Properties>").as_bytes(),
        format!(
            r#"<Properties {CUSTOM_NS}><property fmtid="x" pid="2" name="Client"><vt:lpwstr>&nbsp;x</vt:lpwstr></property></Properties>"#
        )
        .as_bytes(),
    );
    assert_eq!(body_text(&doc), "Body text");
    assert_eq!(
        doc.metadata.core.get("title").map(String::as_str),
        Some("a &bogus; b")
    );
    assert_eq!(
        doc.metadata.custom.get("Client").map(String::as_str),
        Some("&nbsp;x")
    );
}

#[test]
fn cdata_in_document_properties_is_kept_literally() {
    let doc = parse(
        format!(
            "<cp:coreProperties {CORE_NS}><dc:title><![CDATA[R&D <plan>]]></dc:title></cp:coreProperties>"
        )
        .as_bytes(),
        format!("<Properties {APP_NS}><Company><![CDATA[A&B]]></Company></Properties>").as_bytes(),
        format!(
            r#"<Properties {CUSTOM_NS}><property fmtid="x" pid="2" name="Client"><vt:lpwstr><![CDATA[x < y]]></vt:lpwstr></property></Properties>"#
        )
        .as_bytes(),
    );
    assert_eq!(
        doc.metadata.core.get("title").map(String::as_str),
        Some("R&D <plan>")
    );
    assert_eq!(
        doc.metadata.app.get("Company").map(String::as_str),
        Some("A&B")
    );
    assert_eq!(
        doc.metadata.custom.get("Client").map(String::as_str),
        Some("x < y")
    );
}
