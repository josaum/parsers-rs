//! `xs:attributeGroup ref` gives a type the attributes of the group.

use xsd_fast_core::schema::XsdType;
use xsd_fast_core::{ParseOptions, SchemaSet};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:attributeGroup name="Outer">
    <xs:attribute name="x" type="xs:int"/>
    <xs:attributeGroup ref="t:Inner"/>
    <xs:anyAttribute/>
  </xs:attributeGroup>
  <xs:attributeGroup name="Inner">
    <xs:attribute name="y" type="xs:boolean" use="required"/>
    <xs:attributeGroup ref="t:Outer"/>
  </xs:attributeGroup>
  <xs:attributeGroup name="Repeats"><xs:attribute name="x" type="xs:string"/></xs:attributeGroup>
  <xs:complexType name="Base">
    <xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence>
    <xs:attributeGroup ref="t:Outer"/>
    <xs:attributeGroup ref="t:Repeats"/>
  </xs:complexType>
  <xs:complexType name="Derived"><xs:complexContent><xs:extension base="t:Base">
    <xs:attribute name="own" type="xs:string"/>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:element name="root">
    <xs:complexType>
      <xs:sequence><xs:element name="d" type="t:Derived"/></xs:sequence>
      <xs:attribute name="first" type="xs:string"/>
      <xs:attributeGroup ref="t:Inner">
        <xs:annotation><xs:documentation>Inner, again</xs:documentation></xs:annotation>
      </xs:attributeGroup>
      <xs:attributeGroup ref="t:Missing"/>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

fn load() -> SchemaSet {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads")
}

fn names(attributes: &[xsd_fast_core::schema::AttributeDecl]) -> Vec<&str> {
    attributes.iter().map(|a| a.name.as_str()).collect()
}

#[test]
fn a_type_takes_the_attributes_of_its_groups_nested_ones_included() {
    let schema = load();
    let base = &schema.model().complex_types["Base"];
    // `x` comes from two groups and is kept once, with the first one's type.
    assert_eq!(names(&base.attributes), ["x", "y"]);
    assert!(base.attributes[1].required);
    assert_eq!(
        base.attributes[0].type_def.kind,
        xsd_fast_core::SimpleTypeKind::Int
    );
}

#[test]
fn group_attributes_are_inherited() {
    let schema = load();
    let derived = &schema.model().complex_types["Derived"];
    assert_eq!(names(&derived.attributes), ["x", "y", "own"]);
}

#[test]
fn a_group_reached_twice_or_unknown_adds_nothing_more() {
    let schema = load();
    let XsdType::Complex(root) = &schema.model().global_elements["root"].type_def else {
        panic!("root has an inline complex type");
    };
    assert_eq!(names(&root.attributes), ["first", "y", "x"]);
}

#[cfg(feature = "arrow")]
#[test]
fn group_attributes_are_columns() {
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::ArrowOptions;

    let schema = load()
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("root derives");
    let names: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["@first", "@y", "@x", "d"]);
    assert_eq!(
        schema.field_with_name("@y").expect("y").data_type(),
        &DataType::Boolean
    );
    assert!(!schema.field_with_name("@y").expect("y").is_nullable());
}
