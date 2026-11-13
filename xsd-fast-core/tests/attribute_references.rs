//! `xs:attribute ref` takes the declaration of a global attribute, with the
//! `use` written at the reference.

use xsd_fast_core::schema::AttributeDecl;
use xsd_fast_core::{ParseOptions, SchemaSet, SimpleTypeKind};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:simpleType name="Code"><xs:restriction base="xs:int"><xs:maxInclusive value="9"/></xs:restriction></xs:simpleType>
  <xs:attribute name="code" type="t:Code" default="1"/>
  <xs:attribute name="flag" type="xs:boolean" fixed="true"/>
  <xs:attribute name="note"><xs:simpleType><xs:restriction base="xs:date"/></xs:simpleType></xs:attribute>
  <xs:attributeGroup name="AG"><xs:attribute ref="t:flag" use="required"/></xs:attributeGroup>
  <xs:complexType name="Base">
    <xs:attribute ref="t:code"/>
    <xs:attribute ref="t:note" use="required" default="2020-01-01"/>
    <xs:attributeGroup ref="t:AG"/>
    <xs:attribute ref="t:missing"/>
  </xs:complexType>
  <xs:complexType name="Extended"><xs:complexContent><xs:extension base="t:Base">
    <xs:attribute ref="t:code" use="required"/>
  </xs:extension></xs:complexContent></xs:complexType>
  <xs:complexType name="Restricted"><xs:complexContent><xs:restriction base="t:Base">
    <xs:attribute ref="t:note" use="prohibited"/>
  </xs:restriction></xs:complexContent></xs:complexType>
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="b" type="t:Base"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

fn attributes(name: &str) -> Vec<AttributeDecl> {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    schema.model().complex_types[name].attributes.clone()
}

fn names(attributes: &[AttributeDecl]) -> Vec<&str> {
    attributes.iter().map(|a| a.name.as_str()).collect()
}

#[test]
fn a_reference_takes_the_global_declaration_and_the_use_at_the_reference() {
    let base = attributes("Base");
    assert_eq!(names(&base), ["code", "note", "flag"]);
    let [code, note, flag] = base.as_slice() else {
        panic!("three attributes");
    };
    assert_eq!(code.type_def.kind, SimpleTypeKind::Int);
    assert_eq!(code.type_def.facets.max_inclusive.as_deref(), Some("9"));
    assert_eq!(code.default.as_deref(), Some("1"));
    assert!(!code.required);
    assert_eq!(note.type_def.kind, SimpleTypeKind::Date);
    assert!(note.required);
    assert_eq!(note.default.as_deref(), Some("2020-01-01"));
    assert_eq!(flag.type_def.kind, SimpleTypeKind::Boolean);
    assert_eq!(flag.fixed.as_deref(), Some("true"));
    assert!(flag.required);
}

#[test]
fn references_in_extensions_and_restrictions_apply() {
    let extended = attributes("Extended");
    assert_eq!(names(&extended), ["code", "note", "flag"]);
    assert!(extended[0].required);
    let restricted = attributes("Restricted");
    assert_eq!(names(&restricted), ["code", "flag"]);
}

#[cfg(feature = "arrow")]
#[test]
fn referenced_attributes_are_typed_columns() {
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::ArrowOptions;
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("root derives");
    let DataType::Struct(fields) = arrow.field_with_name("b").expect("b").data_type() else {
        panic!("b is a struct");
    };
    let columns: Vec<_> = fields
        .iter()
        .map(|f| (f.name().as_str(), f.data_type().clone(), f.is_nullable()))
        .collect();
    assert_eq!(
        columns,
        [
            ("@code", DataType::Int64, true),
            ("@note", DataType::Date32, false),
            ("@flag", DataType::Boolean, false),
        ]
    );
}

/// A child whose own content derives one column keeps that column, named
/// after the child, as before references were resolved; one with no column
/// of its own gains the referenced attributes, and does not reshape its
/// parent.
#[cfg(feature = "arrow")]
#[test]
fn referenced_attributes_do_not_reshape_existing_columns() {
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::ArrowOptions;
    const SHAPES: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
      <xs:attribute name="id" type="xs:string"/>
      <xs:complexType name="One"><xs:sequence><xs:element name="c" type="xs:int"/></xs:sequence>
        <xs:attribute ref="id"/></xs:complexType>
      <xs:complexType name="Bare"><xs:attribute ref="id"/></xs:complexType>
      <xs:complexType name="Holder"><xs:sequence>
        <xs:element name="v" type="xs:date"/><xs:element name="bare" type="Bare"/>
      </xs:sequence></xs:complexType>
      <xs:element name="root"><xs:complexType><xs:sequence>
        <xs:element name="one" type="One"/>
        <xs:element name="many" type="Bare" maxOccurs="unbounded"/>
        <xs:element name="holder" type="Holder"/>
      </xs:sequence></xs:complexType></xs:element>
    </xs:schema>"#;
    let schema = SchemaSet::load_bytes(SHAPES, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("root derives");
    let data_type = |name: &str| arrow.field_with_name(name).expect(name).data_type().clone();
    assert_eq!(data_type("one"), DataType::Int64);
    let DataType::List(item) = data_type("many") else {
        panic!("many is a list");
    };
    assert!(matches!(item.data_type(), DataType::Struct(f) if f.len() == 1));
    assert_eq!(data_type("holder"), DataType::Date32);
    assert!(arrow.fields().iter().all(|f| f.metadata().is_empty()));
}

#[test]
fn a_model_serialized_without_the_referenced_flag_loads() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let mut json = serde_json::to_value(schema.model()).expect("model serializes");
    fn strip(value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(map.remove("referenced").is_some())
                    + map.values_mut().map(strip).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter_mut().map(strip).sum(),
            _ => 0,
        }
    }
    // Only referenced attributes carry the flag: three in Base, three in
    // Extended (one declared again), two in Restricted.
    assert_eq!(strip(&mut json), 8);
    let older: xsd_fast_core::schema::SchemaModel =
        serde_json::from_value(json).expect("older model loads");
    assert!(older.complex_types["Base"]
        .attributes
        .iter()
        .all(|a| !a.referenced));
}

#[cfg(feature = "arrow")]
#[test]
fn the_shape_option_gives_simple_children_their_attributes() {
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::{ArrowOptions, ArrowShape};
    const SHAPES: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
      <xs:attribute name="id" type="xs:string"/>
      <xs:complexType name="Text"><xs:simpleContent><xs:extension base="xs:int">
        <xs:attribute ref="id"/></xs:extension></xs:simpleContent></xs:complexType>
      <xs:element name="root"><xs:complexType><xs:sequence>
        <xs:element name="text" type="Text"/>
      </xs:sequence></xs:complexType></xs:element>
    </xs:schema>"#;
    let schema = SchemaSet::load_bytes(SHAPES, ParseOptions::default()).expect("schema loads");
    let default = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("root derives");
    assert_eq!(
        default.field_with_name("text").expect("text").data_type(),
        &DataType::Int64
    );
    let shaped = schema
        .to_arrow_schema_with_shape(
            "root",
            ArrowOptions::default(),
            ArrowShape::new().with_attributes_on_simple_children(true),
        )
        .expect("root derives");
    let DataType::Struct(fields) = shaped.field_with_name("text").expect("text").data_type() else {
        panic!("text is a struct of its attribute and its text");
    };
    let names: Vec<_> = fields.iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["@id", "$text"]);
    assert!(shaped.fields().iter().all(|f| f.metadata().is_empty()));
}
