//! A schema's own type named like a built-in, used as a base or attribute type.
//!
//! The decision made for element `type` references holds everywhere a simple
//! type is named: `xs:attribute type`, `xs:restriction base` and the `base`
//! of a simple-content `xs:extension`.

use xsd_fast_core::{ContentModel, ParseOptions, SchemaSet, SimpleTypeKind, ValidateOptions};

const BODY: &str = r#"
  <xs:element name="l" type="Labeled"/>
  <xs:element name="y" type="early"/>
  <xs:complexType name="Labeled">
    <xs:simpleContent><xs:extension base="short"/></xs:simpleContent>
  </xs:complexType>
  <xs:complexType name="Holder">
    <xs:attribute name="size" type="short"/>
    <xs:attribute name="count" type="xs:short"/>
    <xs:attribute name="plain" type="int"/>
  </xs:complexType>
  <xs:simpleType name="early">
    <xs:restriction base="date">
      <xs:enumeration value="1999"/>
      <xs:enumeration value="20000"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="short">
    <xs:restriction base="xs:string">
      <xs:enumeration value="S"/>
      <xs:enumeration value="M"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="date">
    <xs:restriction base="xs:string"><xs:maxLength value="4"/></xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="string">
    <xs:restriction base="string"><xs:maxLength value="2"/></xs:restriction>
  </xs:simpleType>"#;

fn load(schema_attributes: &str, prefix: &str) -> SchemaSet {
    let body = BODY.replace("xs:", prefix);
    let tag = prefix.trim_end_matches(':');
    let tag = if tag.is_empty() {
        "schema".to_string()
    } else {
        format!("{tag}:schema")
    };
    let text = format!("<{tag} {schema_attributes}>{body}</{tag}>");
    SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).expect("schema parses")
}

fn messages(schema: &SchemaSet, document: &str) -> Vec<String> {
    schema
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

#[test]
fn an_unprefixed_base_or_attribute_type_means_the_schemas_own_type() {
    let schema = load("xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"", "xs:");
    let model = schema.model();

    let attributes = &model.complex_types["Holder"].attributes;
    assert_eq!(attributes[0].type_def.kind, SimpleTypeKind::String);
    assert_eq!(attributes[0].type_def.facets.enumerations.len(), 2);
    assert_eq!(attributes[1].type_def.kind, SimpleTypeKind::Short);
    assert!(attributes[1].type_def.facets.enumerations.is_empty());
    assert_eq!(attributes[2].type_def.kind, SimpleTypeKind::Int);

    let ContentModel::SimpleContent(labeled) = &model.complex_types["Labeled"].content else {
        panic!("simple content expected");
    };
    assert_eq!(labeled.kind, SimpleTypeKind::String);
    assert!(messages(&schema, "<l>M</l>").is_empty());
    let refused = messages(&schema, "<l>12</l>");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("not in enumeration"), "{refused:?}");

    // A restriction of the schema's own `date` keeps that type's kind and
    // the facets it does not restate.
    let early = &model.simple_types["early"];
    assert_eq!(early.kind, SimpleTypeKind::String);
    assert_eq!(early.facets.max_length, Some(4));
    assert!(messages(&schema, "<y>1999</y>").is_empty());
    let refused = messages(&schema, "<y>20000</y>");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("maxLength"), "{refused:?}");

    // A type that names itself has only the built-in to mean.
    let own_string = &model.simple_types["string"];
    assert_eq!(own_string.kind, SimpleTypeKind::String);
    assert_eq!(own_string.facets.max_length, Some(2));
    // No marker is left in the model.
    assert!(!serde_json::to_string(model)
        .expect("serializes")
        .contains("\\u0000"));
}

#[test]
fn where_xml_schema_is_the_default_namespace_the_name_is_the_built_in() {
    let schema = load("xmlns=\"http://www.w3.org/2001/XMLSchema\"", "");
    let model = schema.model();
    let attributes = &model.complex_types["Holder"].attributes;
    assert_eq!(attributes[0].type_def.kind, SimpleTypeKind::Short);
    assert!(attributes[0].type_def.facets.enumerations.is_empty());
    let ContentModel::SimpleContent(labeled) = &model.complex_types["Labeled"].content else {
        panic!("simple content expected");
    };
    assert_eq!(labeled.kind, SimpleTypeKind::Short);
    assert_eq!(model.simple_types["early"].kind, SimpleTypeKind::Date);
    assert_eq!(model.simple_types["early"].facets.max_length, None);
}
