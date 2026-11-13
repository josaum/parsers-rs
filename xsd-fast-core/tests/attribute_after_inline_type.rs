//! Attributes declared after an attribute with an inline simple type.
//!
//! This is the shape of the Brazilian NF-e schemas: a `versao` attribute with
//! an inline restriction, followed by a required `Id`.

use xsd_fast_core::{ContentModel, ParseOptions, SchemaSet, SimpleTypeKind};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="TInf">
    <xs:sequence>
      <xs:element name="a" type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="versao" use="required">
      <xs:annotation><xs:documentation>layout version</xs:documentation></xs:annotation>
      <xs:simpleType>
        <xs:restriction base="xs:decimal">
          <xs:enumeration value="4.00"/>
        </xs:restriction>
      </xs:simpleType>
    </xs:attribute>
    <xs:attribute name="Id" type="xs:ID" use="required"/>
    <xs:attribute name="note">
      <xs:simpleType>
        <xs:restriction base="xs:string">
          <xs:maxLength value="9"/>
        </xs:restriction>
      </xs:simpleType>
    </xs:attribute>
    <xs:attribute name="last" type="xs:int"/>
  </xs:complexType>
  <xs:complexType name="After">
    <xs:sequence>
      <xs:element name="b" type="xs:string"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;

#[test]
fn every_attribute_is_kept_and_the_type_after_it_is_read() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let model = schema.model();
    let inf = &model.complex_types["TInf"];
    let names: Vec<&str> = inf.attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["versao", "Id", "note", "last"]);
    assert!(inf.attributes[0].required);
    assert!(inf.attributes[1].required);
    assert!(matches!(&inf.content, ContentModel::Sequence(p) if p.len() == 1));
    // The declaration that follows is still read as its own type.
    let names: Vec<&str> = model.complex_types.keys().map(String::as_str).collect();
    assert_eq!(names, ["TInf", "After"]);
    assert!(
        matches!(&model.complex_types["After"].content, ContentModel::Sequence(p) if p.len() == 1)
    );
}

#[test]
fn an_inline_simple_type_is_the_type_of_its_attribute() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let attributes = &schema.model().complex_types["TInf"].attributes;
    let versao = &attributes[0].type_def;
    assert_eq!(versao.kind, SimpleTypeKind::Decimal);
    let values: Vec<&str> = versao
        .facets
        .enumerations
        .iter()
        .map(|value| value.value.as_str())
        .collect();
    assert_eq!(values, ["4.00"]);
    assert_eq!(attributes[2].type_def.kind, SimpleTypeKind::String);
    assert_eq!(attributes[2].type_def.facets.max_length, Some(9));
    assert_eq!(attributes[3].type_def.kind, SimpleTypeKind::Int);
}
