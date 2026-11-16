//! Slide parts whose bytes are not UTF-8.
//!
//! A byte that is not UTF-8 costs the character it stands in for when it is
//! in text, fails the deck when it is in an attribute the parser reads, and
//! costs nothing anywhere the parser does not look. These outcomes predate the
//! quick-xml 0.42 upgrade, which made its reader reject such bytes wherever
//! they are; the tables below are what the parser produced before it.

use pptx_fast_core::{parse_pptx_bytes, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Builds a one-slide deck whose slide part is `slide`, byte for byte.
fn deck(slide: &[u8]) -> Vec<u8> {
    let parts: Vec<(&str, Vec<u8>)> = vec![
        (
            "_rels/.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        (
            "ppt/presentation.xml",
            format!(
                r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
            )
            .into_bytes(),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/slide" Target="slides/slide1.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        ("ppt/slides/slide1.xml", slide.to_vec()),
    ];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in parts {
        zip.start_file(name, options).unwrap();
        zip.write_all(&body).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// A slide with one shape, its `<p:cSld>` attributes, a prologue before the
/// root and the run text given; every `@` becomes the ISO-8859-1 byte 0xE9.
fn slide(prologue: &str, csld_attrs: &str, text: &str) -> Vec<u8> {
    format!(
        r#"{prologue}<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld{csld_attrs}><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title" descr="x@y"/></p:nvSpPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
    .bytes()
    .map(|byte| if byte == b'@' { 0xE9 } else { byte })
    .collect()
}

/// Parses a deck and renders the outcome: the slide's name and the text of
/// its first paragraph, or `<parse error>`.
fn outcome(slide: &[u8]) -> String {
    match parse_pptx_bytes(deck(slide), ParseOptions::default()) {
        Ok(doc) => {
            let Some(slide) = doc.slides.first() else {
                return "<no slide>".to_string();
            };
            let text = slide
                .shapes
                .first()
                .and_then(|shape| shape.paragraphs.first())
                .map(|paragraph| paragraph.text.clone());
            format!("name={:?} text={:?}", slide.name, text)
        }
        Err(_) => "<parse error>".to_string(),
    }
}

#[test]
fn a_latin_1_byte_in_text_reads_as_a_replacement_character() {
    assert_eq!(
        outcome(&slide("", "", "caf@")),
        "name=None text=Some(\"caf\u{FFFD}\")"
    );
}

#[test]
fn a_declared_latin_1_part_is_still_read_as_utf8() {
    assert_eq!(
        outcome(&slide(
            r#"<?xml version="1.0" encoding="ISO-8859-1"?>"#,
            "",
            "caf@"
        )),
        "name=None text=Some(\"caf\u{FFFD}\")"
    );
}

#[test]
fn a_latin_1_byte_where_the_parser_does_not_look_costs_nothing() {
    // In a comment, in an attribute nobody reads, and in a processing
    // instruction: the deck reads as if the byte were not there.
    assert_eq!(
        outcome(&slide(
            "<!-- caf@ --><?pi caf@?>",
            r#" foo="caf@""#,
            "plain"
        )),
        "name=None text=Some(\"plain\")"
    );
}

#[test]
fn a_latin_1_byte_in_an_attribute_the_parser_reads_fails_the_deck() {
    assert_eq!(
        outcome(&slide("", r#" name="caf@""#, "plain")),
        "<parse error>"
    );
}

#[test]
fn a_utf16_slide_part_reads_as_a_slide_without_text() {
    // The parser does not transcode UTF-16 slides: the markup is not
    // recognised, so the slide is there and holds nothing.
    let utf8 = String::from_utf8_lossy(&slide("", "", "plain")).into_owned();
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in utf8.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(outcome(&utf16), "name=None text=None");
}

/// Parses a deck and renders its slide's name and its first shape's name,
/// or `<parse error>`.
fn names(slide: &[u8]) -> String {
    match parse_pptx_bytes(deck(slide), ParseOptions::default()) {
        Ok(doc) => {
            let Some(slide) = doc.slides.first() else {
                return "<no slide>".to_string();
            };
            let shape = slide.shapes.first().and_then(|shape| shape.name.clone());
            format!("slide={:?} shape={:?}", slide.name, shape)
        }
        Err(_) => "<parse error>".to_string(),
    }
}

/// A slide whose `cSld` and first shape carry the names given, after
/// `prologue`; every `@` becomes the ISO-8859-1 byte 0xE9.
fn named_slide(prologue: &str, slide_name: &str, shape_attrs: &str) -> Vec<u8> {
    format!(
        r#"{prologue}<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld name="{slide_name}"><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" {shape_attrs}/></p:nvSpPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>plain</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
    .bytes()
    .map(|byte| if byte == b'@' { 0xE9 } else { byte })
    .collect()
}

// A U+FFFD the part really holds is a character like any other. Only a
// value whose own bytes are not UTF-8 fails, as on quick-xml 0.41; a stray
// byte elsewhere in the part, before the value or in the same tag, does not
// make a real U+FFFD fail.

#[test]
fn real_replacement_characters_in_names_survive_a_stray_byte_before_them() {
    assert_eq!(
        names(&named_slide(
            "<!-- @ -->",
            "Deck\u{FFFD}",
            "name=\"Title\u{FFFD}\""
        )),
        "slide=Some(\"Deck\u{FFFD}\") shape=Some(\"Title\u{FFFD}\")"
    );
}

#[test]
fn a_real_replacement_character_in_a_shape_name_survives_a_stray_byte_in_its_tag() {
    assert_eq!(
        names(&named_slide(
            "",
            "Deck",
            "name=\"Title\u{FFFD}\" descr=\"x@y\""
        )),
        "slide=Some(\"Deck\") shape=Some(\"Title\u{FFFD}\")"
    );
}

#[test]
fn a_stray_byte_in_a_name_beside_a_real_replacement_character_still_fails() {
    assert_eq!(
        names(&named_slide("", "Deck\u{FFFD}@", "name=\"Title\"")),
        "<parse error>"
    );
    assert_eq!(
        names(&named_slide("", "Deck", "name=\"Title\u{FFFD}@\"")),
        "<parse error>"
    );
}

/// New refusal: a part whose raw bytes fit `max_inflate` but which repair
/// grows past it (each stray byte becomes three) is refused, not read at up
/// to three times the size the limit allowed. quick-xml 0.41 read it.
#[test]
fn a_part_that_repair_grows_past_its_limit_is_refused() {
    let text = "@".repeat(1000);
    let bytes = deck(&slide("", "", &text));
    let options = ParseOptions {
        max_inflate: 3000,
        ..ParseOptions::default()
    };
    let error = parse_pptx_bytes(bytes.clone(), options).expect_err("refused");
    assert!(
        format!("{error:#}").contains("once repaired to UTF-8"),
        "{error:#}"
    );
    // The same part within its limit once repaired still reads.
    let options = ParseOptions {
        max_inflate: 6000,
        ..ParseOptions::default()
    };
    assert!(parse_pptx_bytes(bytes, options).is_ok());
}
