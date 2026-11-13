//! A part encoded in UTF-16 reads the same as its UTF-8 form.

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Utf8,
    /// UTF-16 with a byte order mark.
    Utf16 {
        big_endian: bool,
    },
    /// UTF-16 announced only by the XML declaration.
    Utf16NoMark {
        big_endian: bool,
    },
}

fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
    text.encode_utf16()
        .flat_map(|unit| {
            if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            }
        })
        .collect()
}

/// `body` as a part in `encoding`, with the matching XML declaration.
fn encode(body: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => format!(r#"<?xml version="1.0" encoding="UTF-8"?>{body}"#).into_bytes(),
        Encoding::Utf16 { big_endian } => utf16(
            &format!("\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-16\"?>{body}"),
            big_endian,
        ),
        Encoding::Utf16NoMark { big_endian } => utf16(
            &format!(r#"<?xml version="1.0" encoding="UTF-16"?>{body}"#),
            big_endian,
        ),
    }
}

fn zip(parts: &[(&str, Vec<u8>)], method: CompressionMethod) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
    for (name, bytes) in parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(bytes).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// A document whose every part except the package relationships is in
/// `encoding`.
fn package(encoding: Encoding, method: CompressionMethod) -> Vec<u8> {
    let w = |root: &str, body: &str| {
        encode(
            &format!(r#"<w:{root} xmlns:w="{W}" xmlns:r="{REL}">{body}</w:{root}>"#),
            encoding,
        )
    };
    let text = "caf\u{e9} &amp; \u{65E5}\u{672C} \u{1F600}";
    let run = format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>");
    zip(
        &[
            (
                "_rels/.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
                )
                .into_bytes(),
            ),
            (
                "word/document.xml",
                w(
                    "document",
                    &format!(
                        r#"<w:body><w:p><w:pPr><w:pStyle w:val="Greek"/></w:pPr><w:r><w:t>{text} {symbol}</w:t><w:footnoteReference w:id="2"/></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="rId8"/></w:sectPr></w:body>"#,
                        symbol = '\u{F061}'
                    ),
                ),
            ),
            (
                "word/_rels/document.xml.rels",
                encode(
                    &format!(
                        r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/><Relationship Id="rId3" Type="{REL}/footnotes" Target="footnotes.xml"/><Relationship Id="rId5" Type="{REL}/comments" Target="comments.xml"/><Relationship Id="rId8" Type="{REL}/header" Target="header1.xml"/></Relationships>"#
                    ),
                    encoding,
                ),
            ),
            (
                "word/styles.xml",
                w(
                    "styles",
                    r#"<w:style w:type="paragraph" w:styleId="Greek"><w:name w:val="Gr&#233;&#233;k"/><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:style>"#,
                ),
            ),
            (
                "word/footnotes.xml",
                w("footnotes", &format!(r#"<w:footnote w:id="2">{run}</w:footnote>"#)),
            ),
            (
                "word/comments.xml",
                w(
                    "comments",
                    &format!(r#"<w:comment w:id="1" w:author="Zo&#235; &amp; Co">{run}</w:comment>"#),
                ),
            ),
            ("word/settings.xml", w("settings", r#"<w:zoom w:percent="90"/>"#)),
            ("word/header1.xml", w("hdr", &run)),
            (
                "docProps/core.xml",
                encode(
                    &format!(
                        r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{text}</dc:title></cp:coreProperties>"#
                    ),
                    encoding,
                ),
            ),
            (
                "docProps/custom.xml",
                encode(
                    &format!(
                        r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="x" pid="2" name="Cli&#233;nt"><vt:lpwstr>{text}</vt:lpwstr></property></Properties>"#
                    ),
                    encoding,
                ),
            ),
        ],
        method,
    )
}

fn parse(bytes: Vec<u8>) -> DocxDocument {
    parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses")
}

fn as_json(doc: &DocxDocument) -> serde_json::Value {
    serde_json::to_value(doc).expect("document serializes")
}

#[test]
fn every_utf16_part_reads_like_its_utf8_form() {
    let expected = parse(package(Encoding::Utf8, CompressionMethod::Stored));
    // The fixture is not vacuous: text, a style font, notes, comments,
    // headers and properties all came through.
    let Block::Paragraph(paragraph) = &expected.blocks[0] else {
        panic!("expected a paragraph");
    };
    assert_eq!(
        paragraph.text,
        "caf\u{e9} & \u{65E5}\u{672C} \u{1F600} \u{3B1}"
    );
    assert_eq!(paragraph.style_name.as_deref(), Some("Gr\u{e9}\u{e9}k"));
    assert_eq!(expected.footnotes.len(), 1);
    assert_eq!(
        expected.comments[0].author.as_deref(),
        Some("Zo\u{eb} & Co")
    );
    assert_eq!(expected.headers.len(), 1);
    assert_eq!(
        expected.settings.get("zoom").map(String::as_str),
        Some("percent=90")
    );
    assert!(expected.metadata.core.contains_key("title"));
    assert!(expected.metadata.custom.contains_key("Cli\u{e9}nt"));

    for encoding in [
        Encoding::Utf16 { big_endian: false },
        Encoding::Utf16 { big_endian: true },
        Encoding::Utf16NoMark { big_endian: false },
        Encoding::Utf16NoMark { big_endian: true },
    ] {
        for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
            let doc = parse(package(encoding, method));
            assert_eq!(as_json(&doc), as_json(&expected), "{encoding:?} {method:?}");
        }
    }
}

#[test]
fn damaged_utf16_costs_the_damaged_characters_only() {
    let mut document = encode(
        &format!(
            r#"<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>a@b</w:t></w:r></w:p></w:body></w:document>"#
        ),
        Encoding::Utf16 { big_endian: false },
    );
    // Replace "@" with a high surrogate that nothing follows, and leave half
    // a code unit at the end of the part.
    let at = document
        .windows(2)
        .step_by(2)
        .position(|unit| unit == [b'@', 0])
        .expect("marker present")
        * 2;
    document[at..at + 2].copy_from_slice(&0xD800u16.to_le_bytes());
    document.push(b' ');
    let doc = parse(zip(
        &[("word/document.xml", document)],
        CompressionMethod::Stored,
    ));
    let Block::Paragraph(paragraph) = &doc.blocks[0] else {
        panic!("expected a paragraph");
    };
    assert_eq!(paragraph.text, "a\u{FFFD}b");
}
