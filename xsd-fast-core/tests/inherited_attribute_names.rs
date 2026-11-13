//! A derived type has one attribute of each name: its own declaration wins
//! over the one it inherits.

use xsd_fast_core::{ParseOptions, SchemaSet, SimpleTypeKind};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:attributeGroup name="AG"><xs:attribute name="x" type="xs:int"/></xs:attributeGroup>
  <xs:complexType name="Base">
    <xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence>
    <xs:attributeGroup ref="t:AG"/>
    <xs:attribute name="kept" type="xs:string"/>
    <xs:attribute name="retyped" type="xs:string"/>
  </xs:complexType>
  <xs:complexType name="Again"><xs:complexContent><xs:extension base="t:Base">
    <xs:attributeGroup ref="t:AG"/>
    <xs:attribute name="retyped" type="xs:date"/>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:complexType name="Further"><xs:complexContent><xs:extension base="t:Again">
    <xs:attribute name="x" type="xs:boolean"/>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="again" type="t:Again"/>
    <xs:element name="further" type="t:Further"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

fn attributes(name: &str) -> Vec<(String, SimpleTypeKind)> {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    schema.model().complex_types[name]
        .attributes
        .iter()
        .map(|a| (a.name.clone(), a.type_def.kind.clone()))
        .collect()
}

#[test]
fn an_extension_repeating_its_bases_attributes_has_each_once() {
    assert_eq!(
        attributes("Again"),
        [
            ("x".to_owned(), SimpleTypeKind::Int),
            ("kept".to_owned(), SimpleTypeKind::String),
            ("retyped".to_owned(), SimpleTypeKind::Date),
        ]
    );
}

#[test]
fn the_nearest_declaration_wins_along_a_chain() {
    assert_eq!(
        attributes("Further"),
        [
            ("x".to_owned(), SimpleTypeKind::Boolean),
            ("kept".to_owned(), SimpleTypeKind::String),
            ("retyped".to_owned(), SimpleTypeKind::Date),
        ]
    );
}

#[cfg(feature = "arrow")]
#[test]
fn no_column_name_repeats() {
    use xsd_fast_core::arrow::ArrowOptions;
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("root derives");
    for field in arrow.fields() {
        let arrow_schema::DataType::Struct(children) = field.data_type() else {
            panic!("{} is a struct", field.name());
        };
        let mut names: Vec<_> = children.iter().map(|c| c.name().clone()).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "{}", field.name());
    }
}
