//! Named types written as self-closing tags.
//!
//! `<xs:complexType name="x"/>` declares a type with no content and
//! `<xs:simpleType name="x"/>` one with no restriction. Both are recorded, so
//! a reference to them resolves like a reference to any other named type.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="Nothing"/>
  <xs:simpleType name="Anything"/>
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="marker" type="Nothing"/>
    <xs:element name="text" type="Anything"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

#[test]
fn self_closing_named_types_are_recorded() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let model = schema.model();
    assert!(model.complex_types.contains_key("Nothing"));
    assert!(model.simple_types.contains_key("Anything"));
    assert!(model.resolve_type("Nothing").is_some());
    assert!(model.resolve_type("Anything").is_some());
}

#[test]
fn a_document_using_them_validates() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let findings = schema
        .validate(
            b"<root><marker/><text>free</text></root>",
            ValidateOptions::default(),
        )
        .expect("validation runs");
    assert!(findings.is_empty(), "{findings:?}");
}
