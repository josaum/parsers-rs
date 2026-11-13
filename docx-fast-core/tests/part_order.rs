//! Parts are walked in name order, whatever order the archive lists them in.
//!
//! Images are collected from every relationships part. The order they come
//! out in, and which part is blamed when two of them are broken, must not
//! depend on the archive's own order or on a hash map's.

use std::io::{Cursor, Write};

use docx_fast_core::{parse_docx_bytes, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn image_rels(entries: &[(&str, &str)]) -> String {
    let mut xml =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS}">"#);
    for (id, target) in entries {
        xml.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{R}/image" Target="{target}"/>"#
        ));
    }
    xml.push_str("</Relationships>");
    xml
}

/// A document plus `extra` parts, written in the order given.
fn package(extra: &[(&str, String)], reversed: bool) -> Vec<u8> {
    let mut parts = vec![
        (
            "_rels/.rels",
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="{R}/officeDocument" Target="word/document.xml"/></Relationships>"#
            ),
        ),
        (
            "word/document.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#.to_string(),
        ),
    ];
    parts.extend(extra.iter().map(|(name, body)| (*name, body.clone())));
    if reversed {
        parts.reverse();
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in &parts {
        zip.start_file(*name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn images_come_out_in_part_then_id_order() {
    let extra = [
        (
            "word/_rels/header1.xml.rels",
            image_rels(&[("rId2", "media/logo.png"), ("rId1", "media/seal.jpeg")]),
        ),
        (
            "word/_rels/document.xml.rels",
            image_rels(&[("rId9", "media/chart.png"), ("rId3", "media/photo.jpeg")]),
        ),
    ];
    let expected = [
        ("word/document.xml", "rId3", "word/media/photo.jpeg"),
        ("word/document.xml", "rId9", "word/media/chart.png"),
        ("word/header1.xml", "rId1", "word/media/seal.jpeg"),
        ("word/header1.xml", "rId2", "word/media/logo.png"),
    ];
    for reversed in [false, true] {
        let parsed = parse_docx_bytes(package(&extra, reversed), ParseOptions::default()).unwrap();
        let images: Vec<(&str, &str, &str)> = parsed
            .images
            .iter()
            .map(|image| {
                (
                    image.source_part.as_str(),
                    image.id.as_str(),
                    image.target.as_str(),
                )
            })
            .collect();
        assert_eq!(images, expected, "archive reversed: {reversed}");
    }
}

#[test]
fn of_two_broken_relationship_parts_the_first_by_name_is_reported() {
    let extra = [
        (
            "word/_rels/header1.xml.rels",
            format!(r#"<Relationships xmlns="{RELS}"><Relationship></second>"#),
        ),
        (
            "word/_rels/footer1.xml.rels",
            format!(r#"<Relationships xmlns="{RELS}"><Relationship></first>"#),
        ),
    ];
    for reversed in [false, true] {
        let error = parse_docx_bytes(package(&extra, reversed), ParseOptions::default())
            .expect_err("broken relationships");
        let error = format!("{error:#}");
        assert!(
            error.contains("</first>"),
            "archive reversed {reversed}: {error}"
        );
        assert!(
            !error.contains("</second>"),
            "archive reversed {reversed}: {error}"
        );
    }
}
