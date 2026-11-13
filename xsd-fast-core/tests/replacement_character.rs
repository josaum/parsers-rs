//! A U+FFFD a schema or document really holds is read as text.
//!
//! Bytes that are not UTF-8 are read as U+FFFD, and a name or value in which
//! one was replaced stays unreadable, as it was when the reader handed over
//! raw bytes. That must not spread to a U+FFFD the document holds itself
//! because a stray byte sits somewhere else, say in a comment. The expected
//! values were taken from the parser on quick-xml 0.41.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// Puts byte 0xFF in place of each `~`.
fn with_stray_byte(text: &str) -> Vec<u8> {
    text.bytes()
        .map(|b| if b == b'~' { 0xFF } else { b })
        .collect()
}

/// The schema model and the findings for the instance, as JSON.
fn read(schema: &[u8], instance: &[u8]) -> String {
    let schema = match SchemaSet::load_bytes(schema, ParseOptions::default()) {
        Ok(schema) => schema,
        Err(error) => return format!("schema error: {error}"),
    };
    let model = serde_json::to_string(schema.model()).expect("model serializes");
    let findings = match schema.validate(instance, ValidateOptions::default()) {
        Ok(findings) => serde_json::to_string(&findings).expect("findings serialize"),
        Err(error) => format!("validation error: {error}"),
    };
    format!("{model}\n{findings}")
}

/// A type named with U+FFFD, and a stray byte in a comment before it. The
/// type is kept, so a value outside its enumeration is still reported.
#[test]
fn type_named_with_replacement_character_is_kept_beside_a_stray_byte() {
    let schema = "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><!-- ~ -->\
        <xs:simpleType name=\"t\u{FFFD}\"><xs:restriction base=\"xs:string\">\
        <xs:enumeration value=\"A\"/><xs:enumeration value=\"B\"/>\
        </xs:restriction></xs:simpleType>\
        <xs:element name=\"v\" type=\"t\u{FFFD}\"/></xs:schema>";
    let clean = read(&with_stray_byte(&schema.replace('~', "x")), b"<v>Z</v>");
    let stray = read(&with_stray_byte(schema), b"<v>Z</v>");
    assert_eq!(stray, clean);
    assert!(stray.contains("not in enumeration"), "{stray}");
}

/// An enumeration value and instance text holding U+FFFD, with stray bytes
/// in comments of both documents, are read as written.
#[test]
fn enumeration_and_text_with_replacement_character_are_kept() {
    let schema = "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><!-- ~ -->\
        <xs:simpleType name=\"t\"><xs:restriction base=\"xs:string\">\
        <xs:enumeration value=\"A\"/><xs:enumeration value=\"B\u{FFFD}\"/>\
        </xs:restriction></xs:simpleType>\
        <xs:element name=\"v\" type=\"t\"/></xs:schema>";
    let instance = "<!-- ~ --><v>B\u{FFFD}</v>";
    let output = read(&with_stray_byte(schema), &with_stray_byte(instance));
    assert_eq!(output, EXPECTED_ENUMERATION_KEPT);
}

/// A stray byte inside the enumeration value or the text itself still makes
/// that value unreadable, as it always did.
#[test]
fn stray_byte_in_the_value_itself_is_still_unreadable() {
    let schema = "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\
        <xs:simpleType name=\"t\"><xs:restriction base=\"xs:string\">\
        <xs:enumeration value=\"A\"/><xs:enumeration value=\"B~\"/>\
        </xs:restriction></xs:simpleType>\
        <xs:element name=\"v\" type=\"t\"/></xs:schema>";
    let output = read(&with_stray_byte(schema), &with_stray_byte("<v>B~</v>"));
    assert_eq!(output, EXPECTED_STRAY_IN_VALUE);
}

const EXPECTED_ENUMERATION_KEPT: &str = "{\"target_namespace\":null,\"complex_types\":{},\"simple_types\":{\"t\":{\"name\":{\"namespace\":null,\"local\":\"t\"},\"kind\":\"String\",\"facets\":{\"enumerations\":[{\"value\":\"A\",\"documentation\":null},{\"value\":\"B\u{FFFD}\",\"documentation\":null}],\"pattern\":null,\"min_length\":null,\"max_length\":null,\"min_inclusive\":null,\"max_inclusive\":null,\"min_exclusive\":null,\"max_exclusive\":null,\"total_digits\":null,\"fraction_digits\":null}}},\"global_elements\":{\"v\":{\"name\":{\"namespace\":null,\"local\":\"v\"},\"type_def\":{\"Ref\":{\"namespace\":null,\"local\":\"t\"}},\"occurrence\":{\"min\":1,\"max\":1},\"nillable\":false,\"documentation\":null}}}\n[]";
const EXPECTED_STRAY_IN_VALUE: &str = "{\"target_namespace\":null,\"complex_types\":{},\"simple_types\":{\"t\":{\"name\":{\"namespace\":null,\"local\":\"t\"},\"kind\":\"String\",\"facets\":{\"enumerations\":[{\"value\":\"A\",\"documentation\":null},{\"value\":\"\",\"documentation\":null}],\"pattern\":null,\"min_length\":null,\"max_length\":null,\"min_inclusive\":null,\"max_inclusive\":null,\"min_exclusive\":null,\"max_exclusive\":null,\"total_digits\":null,\"fraction_digits\":null}}},\"global_elements\":{\"v\":{\"name\":{\"namespace\":null,\"local\":\"v\"},\"type_def\":{\"Ref\":{\"namespace\":null,\"local\":\"t\"}},\"occurrence\":{\"min\":1,\"max\":1},\"nillable\":false,\"documentation\":null}}}\n[]";
