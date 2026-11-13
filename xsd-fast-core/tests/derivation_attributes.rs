//! Attributes declared inside an extension or a restriction.
//!
//! `xs:attribute` is a child of `xs:extension` and `xs:restriction` under
//! both `xs:complexContent` and `xs:simpleContent`; all four are read.

use xsd_fast_core::schema::ContentModel;
use xsd_fast_core::{ParseOptions, SchemaSet};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="Base">
    <xs:sequence><xs:element name="inherited" type="xs:string"/></xs:sequence>
    <xs:attribute name="base_attr" type="xs:string"/>
  </xs:complexType>
  <xs:complexType name="ComplexExtension"><xs:complexContent><xs:extension base="Base">
    <xs:sequence><xs:element name="own" type="xs:string"/></xs:sequence>
    <xs:attribute name="closed" type="xs:int" use="required"/>
    <xs:attribute name="open"><xs:simpleType><xs:restriction base="xs:string">
      <xs:enumeration value="a"/><xs:enumeration value="b"/>
    </xs:restriction></xs:simpleType></xs:attribute>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:complexType name="AttributesOnly"><xs:complexContent><xs:extension base="Base">
    <xs:attribute name="only" type="xs:string"/>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:complexType name="ComplexRestriction"><xs:complexContent><xs:restriction base="xs:anyType">
    <xs:sequence><xs:element name="kept" type="xs:string"/></xs:sequence>
    <xs:attribute name="unit" type="xs:string" use="required"/>
  </xs:restriction></xs:complexContent></xs:complexType>
  <xs:complexType name="SimpleExtension"><xs:simpleContent><xs:extension base="xs:decimal">
    <xs:attribute name="currency" type="xs:string" use="required"/>
    <xs:attribute name="scale" type="xs:int"/>
  </xs:extension></xs:simpleContent></xs:complexType>
  <xs:complexType name="SimpleRestriction"><xs:simpleContent><xs:restriction base="xs:string">
    <xs:maxLength value="3"/>
    <xs:attribute name="lang" type="xs:string"/>
  </xs:restriction></xs:simpleContent></xs:complexType>
</xs:schema>"#;

fn attributes(type_name: &str) -> Vec<(String, bool)> {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    schema.model().complex_types[type_name]
        .attributes
        .iter()
        .map(|attribute| (attribute.name.clone(), attribute.required))
        .collect()
}

fn named(names: &[(&str, bool)]) -> Vec<(String, bool)> {
    names
        .iter()
        .map(|(name, required)| ((*name).to_string(), *required))
        .collect()
}

#[test]
fn complex_content_extension_attributes_follow_the_inherited_ones() {
    assert_eq!(
        attributes("ComplexExtension"),
        named(&[("base_attr", false), ("closed", true), ("open", false)])
    );
    assert_eq!(
        attributes("AttributesOnly"),
        named(&[("base_attr", false), ("only", false)])
    );
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let extension = &schema.model().complex_types["ComplexExtension"];
    assert_eq!(
        extension.attributes[2].type_def.facets.enumerations.len(),
        2
    );
    let ContentModel::Sequence(particles) = &extension.content else {
        panic!("expected a sequence");
    };
    assert_eq!(particles.len(), 2);
}

#[test]
fn complex_content_restriction_keeps_its_particles_and_attributes() {
    assert_eq!(attributes("ComplexRestriction"), named(&[("unit", true)]));
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let restriction = &schema.model().complex_types["ComplexRestriction"];
    let ContentModel::Sequence(particles) = &restriction.content else {
        panic!("expected a sequence, got {:?}", restriction.content);
    };
    assert_eq!(particles.len(), 1);
    assert!(restriction.base.is_none());
}

#[test]
fn simple_content_extension_and_restriction_attributes_are_read() {
    assert_eq!(
        attributes("SimpleExtension"),
        named(&[("currency", true), ("scale", false)])
    );
    assert_eq!(attributes("SimpleRestriction"), named(&[("lang", false)]));
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    for name in ["SimpleExtension", "SimpleRestriction"] {
        assert!(
            matches!(
                schema.model().complex_types[name].content,
                ContentModel::SimpleContent(_)
            ),
            "{name}"
        );
    }
}
