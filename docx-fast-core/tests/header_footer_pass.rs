//! What a document's header and footer references decide, pinned.
//!
//! The references are found in the document part before the body is parsed,
//! and the headers and footers they name are parsed before the body too. That
//! order shows in two places: a malformed document reports the error of the
//! reference search, wherever the damage is, and fields are numbered through
//! the headers and footers first and the body after.

use std::io::{Cursor, Write};

use docx_fast_core::{parse_docx_bytes, DocxDocument, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

const BODY_FIELD: &str = r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>DATE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>today</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
const SECTION: &str = r#"<w:sectPr><w:headerReference w:type="default" r:id="rId1"/><w:footerReference w:type="default" r:id="rId2"/></w:sectPr>"#;
const PLAIN_FOOTER: &str = "<w:p><w:r><w:t>Footer text</w:t></w:r></w:p>";
const FIELD_FOOTER: &str = r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;

/// A document whose body is `body`, with one header and one footer part.
fn document(body: &str, footer: &str) -> Vec<u8> {
    let parts = [
        (
            "_rels/.rels".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{R}/officeDocument" Target="word/document.xml"/></Relationships>"#
            ),
        ),
        (
            "word/_rels/document.xml.rels".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{R}/header" Target="header1.xml"/><Relationship Id="rId2" Type="{R}/footer" Target="footer1.xml"/></Relationships>"#
            ),
        ),
        (
            "word/document.xml".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="{W}" xmlns:r="{R}"><w:body>{body}</w:body></w:document>"#
            ),
        ),
        (
            "word/header1.xml".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><w:hdr xmlns:w="{W}"><w:p><w:r><w:t>Header text</w:t></w:r></w:p></w:hdr>"#
            ),
        ),
        (
            "word/footer1.xml".to_string(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><w:ftr xmlns:w="{W}">{footer}</w:ftr>"#
            ),
        ),
    ];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in &parts {
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn parse(bytes: Vec<u8>) -> anyhow::Result<DocxDocument> {
    parse_docx_bytes(bytes, ParseOptions::default())
}

fn error_of(body: &str) -> String {
    let error = parse(document(body, PLAIN_FOOTER)).expect_err("malformed document");
    format!("{error:#}")
}

const PARAGRAPH: &str = "<w:p><w:r><w:t>Text</w:t></w:r></w:p>";
/// An end tag that closes nothing that is open.
const BREAK: &str = "<w:p></w:tbl>";

#[test]
fn a_document_broken_before_the_section_properties_reports_the_reference_search() {
    let error = error_of(&format!("{PARAGRAPH}{BREAK}{PARAGRAPH}{SECTION}"));
    assert_eq!(error, EXPECTED_ERROR);
}

#[test]
fn a_document_broken_inside_the_section_properties_reports_the_reference_search() {
    let section = r#"<w:sectPr><w:headerReference w:type="default" r:id="rId1"/></w:tbl><w:footerReference w:type="default" r:id="rId2"/></w:sectPr>"#;
    let error = error_of(&format!("{PARAGRAPH}{section}"));
    assert_eq!(error, EXPECTED_ERROR_IN_SECTION);
}

#[test]
fn a_document_broken_after_the_section_properties_reports_the_reference_search() {
    let error = error_of(&format!("{PARAGRAPH}{SECTION}{BREAK}"));
    assert_eq!(error, EXPECTED_ERROR);
}

#[test]
fn a_well_formed_document_has_its_header_and_footer() {
    let parsed = parse(document(&format!("{PARAGRAPH}{SECTION}"), PLAIN_FOOTER)).unwrap();
    assert_eq!(parsed.headers.len(), 1);
    assert_eq!(parsed.footers.len(), 1);
    assert_eq!(parsed.headers[0].part, "word/header1.xml");
    assert_eq!(parsed.footers[0].part, "word/footer1.xml");
    assert_eq!(parsed.headers[0].ref_type.as_deref(), Some("default"));
    assert_eq!(parsed.blocks.len(), 1);
}

#[test]
fn fields_are_numbered_through_the_footer_before_the_body() {
    let body = format!("{BODY_FIELD}{BODY_FIELD}{SECTION}");
    let parsed = parse(document(&body, FIELD_FOOTER)).unwrap();
    let fields: Vec<(u32, &str)> = parsed
        .fields
        .iter()
        .map(|field| (field.id, field.instr.as_deref().unwrap_or("").trim()))
        .collect();
    assert_eq!(fields, [(1, "PAGE"), (2, "DATE"), (3, "DATE")]);
    let json = serde_json::to_string(&parsed.blocks).unwrap();
    assert!(json.contains(r#""field_id":2"#), "{json}");
    assert!(json.contains(r#""field_id":3"#), "{json}");
    assert!(!json.contains(r#""field_id":1"#), "{json}");
}

#[test]
fn fields_are_numbered_from_one_when_the_footer_has_none() {
    let body = format!("{BODY_FIELD}{BODY_FIELD}{SECTION}");
    let parsed = parse(document(&body, PLAIN_FOOTER)).unwrap();
    let ids: Vec<u32> = parsed.fields.iter().map(|field| field.id).collect();
    assert_eq!(ids, [1, 2]);
    assert_eq!(parsed.footers.len(), 1);
}

const EXPECTED_ERROR: &str =
    "parse header/footer refs: ill-formed document: expected `</w:p>`, but `</w:tbl>` was found";
const EXPECTED_ERROR_IN_SECTION: &str =
    "parse header/footer refs: ill-formed document: expected `</w:sectPr>`, but `</w:tbl>` was found";
