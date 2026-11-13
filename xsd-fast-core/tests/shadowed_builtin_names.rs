//! A schema's own simple type named like a built-in (`short`, `string`, `date`).
//!
//! A type reference is resolved by namespace. A reference without a prefix
//! names the schema's own type when the schema declares one; only where the
//! XML Schema namespace is the default namespace does it name the built-in.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, XsdType};

const TYPES: &str = r#"
  <xs:simpleType name="short">
    <xs:restriction base="xs:string">
      <xs:enumeration value="S"/>
      <xs:enumeration value="M"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="date">
    <xs:restriction base="xs:string">
      <xs:maxLength value="4"/>
    </xs:restriction>
  </xs:simpleType>"#;

const ELEMENTS: &str = r#"
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="size" type="short" minOccurs="0"/>
        <xs:element name="year" type="date" minOccurs="0"/>
        <xs:element name="count" type="xs:short" minOccurs="0"/>
        <xs:element name="plain" type="int" minOccurs="0"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
  <xs:element name="top" type="short"/>"#;

fn messages(schema: &SchemaSet, document: &str) -> Vec<String> {
    schema
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

fn check(schema: &SchemaSet) {
    assert_eq!(
        messages(schema, "<r><size>M</size><year>1999</year></r>"),
        Vec::<String>::new()
    );
    let refused = messages(schema, "<r><size>12</size><year>1999-01-01</year></r>");
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(refused[0].contains("not in enumeration"), "{refused:?}");
    assert!(refused[1].contains("maxLength"), "{refused:?}");
    let refused = messages(schema, "<top>XL</top>");
    assert_eq!(refused.len(), 1, "{refused:?}");
    // A prefixed reference to the XML Schema namespace is still the built-in,
    // and an unprefixed name the schema does not declare is still the built-in.
    let refused = messages(schema, "<r><count>x</count><plain>y</plain></r>");
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(
        refused.iter().all(|m| m.contains("expected numeric")),
        "{refused:?}"
    );
}

#[test]
fn an_unprefixed_reference_means_the_schemas_own_type() {
    // Declared before use, and after use (a forward reference).
    for body in [format!("{TYPES}{ELEMENTS}"), format!("{ELEMENTS}{TYPES}")] {
        let text = format!(
            "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\" xmlns=\"urn:own\" targetNamespace=\"urn:own\">{body}</xs:schema>"
        );
        let schema =
            SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).expect("schema parses");
        check(&schema);
    }
}

#[test]
fn the_own_type_may_come_from_an_included_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("types.xsd"),
        format!("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">{TYPES}</xs:schema>"),
    )
    .expect("write");
    std::fs::write(
        dir.path().join("root.xsd"),
        format!(
            "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">{ELEMENTS}<xs:include schemaLocation=\"types.xsd\"/></xs:schema>"
        ),
    )
    .expect("write");
    let schema =
        SchemaSet::load(dir.path().join("root.xsd"), ParseOptions::default()).expect("loads");
    check(&schema);
}

#[test]
fn where_xml_schema_is_the_default_namespace_an_unprefixed_name_is_the_built_in() {
    let text = format!(
        "<schema xmlns=\"http://www.w3.org/2001/XMLSchema\" xmlns:xs=\"http://www.w3.org/2001/XMLSchema\" xmlns:o=\"urn:own\" targetNamespace=\"urn:own\">{TYPES}\
         <element name=\"n\" type=\"short\"/><element name=\"own\" type=\"o:short\"/></schema>"
    );
    let schema =
        SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).expect("schema parses");
    assert!(messages(&schema, "<n>12</n>").is_empty());
    assert_eq!(messages(&schema, "<n>M</n>").len(), 1);
    assert!(messages(&schema, "<own>M</own>").is_empty());
    assert_eq!(messages(&schema, "<own>12</own>").len(), 1);
}

#[test]
fn a_schema_without_such_a_type_reads_as_before() {
    let text = "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"n\" type=\"short\"/></xs:schema>";
    let schema =
        SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).expect("schema parses");
    let declared = &schema.model().global_elements["n"].type_def;
    assert!(
        matches!(declared, XsdType::Simple(simple) if simple.name.is_none() && simple.kind.is_numeric()),
        "{declared:?}"
    );
}
