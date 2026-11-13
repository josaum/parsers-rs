//! A presentation reads `docProps/app.xml` exactly as a Word document does.
//!
//! The same part goes into a deck and into a document, and the two parsers
//! must give the same extended properties: nested vectors keyed by the
//! property that holds them, one line per item, empty items included.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// An extended-properties part as PowerPoint writes it, indented, with the
/// cases that used to read differently: vectors of variants and of strings,
/// empty items written both ways, line breaks inside an item, a reference,
/// and a property holding only the indentation of an element with no content.
const APP_XML: &[u8] = include_bytes!("fixtures/app.xml");

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(body).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// The extended properties a one-slide deck with `app` reads.
fn deck_app(app: &[u8]) -> HashMap<String, String> {
    let root_rels = format!(
        r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let presentation_rels = format!(
        r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/slide" Target="slides/slide1.xml"/></Relationships>"#
    );
    let slide = format!(r#"<p:sld xmlns:p="{P}"><p:cSld><p:spTree/></p:cSld></p:sld>"#);
    let bytes = package(&[
        ("_rels/.rels", root_rels.as_bytes()),
        ("ppt/presentation.xml", presentation.as_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            presentation_rels.as_bytes(),
        ),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
        ("docProps/app.xml", app),
    ]);
    pptx_fast_core::parse_pptx_bytes(bytes, pptx_fast_core::ParseOptions::default())
        .expect("deck parses")
        .metadata
        .app
}

/// The extended properties a one-paragraph document with `app` reads.
fn document_app(app: &[u8]) -> HashMap<String, String> {
    let document = format!(r#"<w:document xmlns:w="{W}"><w:body><w:p/></w:body></w:document>"#);
    let bytes = package(&[
        ("word/document.xml", document.as_bytes()),
        ("docProps/app.xml", app),
    ]);
    docx_fast_core::parse_docx_bytes(bytes, docx_fast_core::ParseOptions::default())
        .expect("document parses")
        .metadata
        .app
}

fn sorted(map: &HashMap<String, String>) -> Vec<(&str, &str)> {
    let mut pairs: Vec<_> = map
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
}

#[test]
fn a_deck_reads_app_properties_as_a_document_does() {
    let deck = deck_app(APP_XML);
    assert_eq!(deck, document_app(APP_XML));
    assert_eq!(
        sorted(&deck),
        [
            ("AppVersion", "16.0000"),
            ("Application", "Microsoft Office PowerPoint"),
            ("Company", "Acme & Co"),
            ("HeadingPairs", "Fonts Used\n2\nTheme\n1\nSlide Titles\n4"),
            ("PresentationFormat", "Widescreen"),
            ("Slides", "4"),
            (
                "TitlesOfParts",
                "Aptos\nArial\nOffice Theme\nQ3 plan: two lines\n\n\nR&D budget"
            ),
            ("TotalTime", "12"),
            ("Words", "240"),
        ]
    );
}
