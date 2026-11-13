//! What a document's header and footer references decide, pinned.
//!
//! The references are found in the document part before the body is parsed,
//! and the headers and footers they name are parsed before the body too. That
//! order shows in two places: a malformed document reports the error of the
//! reference search, wherever the damage is, and fields are numbered through
//! the headers and footers first and the body after.
//!
//! A field's number is one more than the count of fields already finished
//! when it begins. It is not a unique identifier: a field nested in another
//! begins before the outer one is finished and shares its number, and a
//! field that is never closed numbers its runs without a `Field` to match.
//! That is how the parser has always numbered them; the tests at the end pin
//! it so that a change to the parse order cannot alter it unnoticed.

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

/// Thirty-three tables, one inside the other: one more than the parser allows.
fn tables_too_deep() -> String {
    format!(
        "{}{}",
        "<w:tbl><w:tr><w:tc>".repeat(33),
        "</w:tc></w:tr></w:tbl>".repeat(33)
    )
}

/// Block-parser runs and reference searches spent on parsing `bytes`.
fn work(bytes: Vec<u8>) -> (bool, u64, u64) {
    let before = docx_fast_core::pass_counts();
    let parsed = parse(bytes).is_ok();
    let after = docx_fast_core::pass_counts();
    (parsed, after.0 - before.0, after.1 - before.1)
}

#[test]
fn a_well_formed_document_is_read_once_and_never_searched() {
    let bytes = document(&format!("{PARAGRAPH}{SECTION}"), PLAIN_FOOTER);
    // Body, header, footer; the references come out of the body parse.
    assert_eq!(work(bytes), (true, 3, 0));
}

#[test]
fn a_body_the_parser_rejects_is_not_parsed_a_second_time() {
    let body = format!("{PARAGRAPH}{SECTION}{}", tables_too_deep());
    let bytes = document(&body, PLAIN_FOOTER);
    let error = format!("{:#}", parse(bytes.clone()).expect_err("too deep"));
    assert!(error.contains("nested"), "{error}");
    // Body (fails), then what could fail first in the original order: one
    // reference search, the header and the footer. Not the body again.
    assert_eq!(work(bytes), (false, 3, 1));
}

#[test]
fn an_ill_formed_document_is_rejected_after_one_read() {
    let bytes = document(&format!("{PARAGRAPH}{SECTION}{BREAK}"), PLAIN_FOOTER);
    // The body parse meets the damage; that is the reference search's error
    // too, so nothing else is read.
    assert_eq!(work(bytes), (false, 1, 0));
}

/// A PAGE field inside the instruction of an IF field.
const NESTED_FIELD: &str = r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>IF </w:instrText></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>3</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:instrText> = 1</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>outer</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
/// A field that begins and is never closed.
const UNCLOSED_FIELD: &str = r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>AUTHOR</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>nobody</w:t></w:r></w:p>"#;

/// The numbers of the fields, and the field number of every body run that
/// has one, in document order (tables included).
fn field_numbers(parsed: &DocxDocument) -> (Vec<u32>, Vec<u32>) {
    fn runs(value: &serde_json::Value, out: &mut Vec<u32>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(id) = map.get("field_id").and_then(serde_json::Value::as_u64) {
                    out.push(u32::try_from(id).unwrap());
                }
                for (key, child) in map {
                    if key != "field_id" {
                        runs(child, out);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| runs(item, out)),
            _ => {}
        }
    }
    let mut in_runs = Vec::new();
    // `blocks` is an array of blocks whose runs are arrays: order is kept.
    runs(&serde_json::to_value(&parsed.blocks).unwrap(), &mut in_runs);
    (
        parsed.fields.iter().map(|field| field.id).collect(),
        in_runs,
    )
}

#[test]
fn nested_and_unclosed_fields_keep_their_numbers() {
    let body = format!(
        "{BODY_FIELD}{NESTED_FIELD}<w:tbl><w:tr><w:tc>{BODY_FIELD}</w:tc></w:tr></w:tbl>{UNCLOSED_FIELD}{SECTION}"
    );
    let plain = parse(document(&body, PLAIN_FOOTER)).unwrap();
    let (fields, runs) = field_numbers(&plain);
    assert_eq!(
        (fields.as_slice(), runs.as_slice()),
        (EXPECTED_FIELDS, EXPECTED_RUNS)
    );

    // One field in the footer comes first and moves every body number by one.
    let shifted = parse(document(&body, FIELD_FOOTER)).unwrap();
    let (shifted_fields, shifted_runs) = field_numbers(&shifted);
    let mut expected_fields = vec![1];
    expected_fields.extend(fields.iter().map(|id| id + 1));
    assert_eq!(shifted_fields, expected_fields);
    assert_eq!(
        shifted_runs,
        runs.iter().map(|id| id + 1).collect::<Vec<u32>>()
    );
}

const EXPECTED_FIELDS: &[u32] = &[1, 2, 2, 4];
const EXPECTED_RUNS: &[u32] = &[1, 2, 2, 4, 5];

const EXPECTED_ERROR: &str =
    "parse header/footer refs: ill-formed document: expected `</w:p>`, but `</w:tbl>` was found";
const EXPECTED_ERROR_IN_SECTION: &str =
    "parse header/footer refs: ill-formed document: expected `</w:sectPr>`, but `</w:tbl>` was found";
