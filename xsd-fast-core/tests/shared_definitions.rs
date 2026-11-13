//! What types share instead of copying, and what expansion may copy.

use std::sync::Arc;

use xsd_fast_core::schema::XsdType;
use xsd_fast_core::{ParseOptions, SchemaSet};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:simpleType name="Big"><xs:restriction base="xs:string">
    <xs:enumeration value="a"/><xs:enumeration value="b"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="U"><xs:union memberTypes="t:Big xs:int"/></xs:simpleType>
  <xs:element name="root"><xs:complexType>
    <xs:attribute name="one" type="t:Big"/>
    <xs:attribute name="two" type="t:Big"/>
    <xs:attribute name="three" type="t:U"/>
    <xs:attribute name="four" type="t:U"/>
  </xs:complexType></xs:element>
</xs:schema>"#;

#[test]
fn types_naming_a_simple_type_share_its_facets_and_members() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let model = schema.model();
    let XsdType::Complex(root) = &model.global_elements["root"].type_def else {
        panic!("root has an inline complex type");
    };
    let attributes = &root.attributes;
    let big = &model.simple_types["Big"];
    assert!(Arc::ptr_eq(&attributes[0].type_def.facets, &big.facets));
    assert!(Arc::ptr_eq(&attributes[1].type_def.facets, &big.facets));
    assert!(Arc::ptr_eq(
        &attributes[2].type_def.union_members,
        &attributes[3].type_def.union_members
    ));
    assert!(Arc::ptr_eq(
        &attributes[2].type_def.union_members[0].facets,
        &big.facets
    ));
}

#[test]
fn copied_attribute_text_is_bounded() {
    // 64 types each referencing a group whose attribute has a 1 MiB fixed
    // value: copying it into every type would take 64 MiB.
    let fixed = "x".repeat(1 << 20);
    let mut schema = format!(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t"><xs:attributeGroup name="G"><xs:attribute name="a" type="xs:string" fixed="{fixed}"/></xs:attributeGroup>"#
    );
    for index in 0..64 {
        schema.push_str(&format!(
            r#"<xs:complexType name="c{index}"><xs:attributeGroup ref="t:G"/></xs:complexType>"#
        ));
    }
    schema.push_str("</xs:schema>");
    let options = ParseOptions {
        max_schema_bytes: 1 << 26,
        ..ParseOptions::default()
    };
    let schema = SchemaSet::load_bytes(schema.as_bytes(), options).expect("schema loads");
    let copies = schema
        .model()
        .complex_types
        .values()
        .filter(|ct| !ct.attributes.is_empty())
        .count();
    // 16 MiB of copied text holds 15 copies of the value and its name.
    assert_eq!(copies, 15);
}
