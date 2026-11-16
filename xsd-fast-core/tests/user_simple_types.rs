//! Named simple types that derive from other named simple types.
//!
//! An element or attribute typed with a schema's own simple type takes the
//! kind and the facets of the whole chain of restrictions below it. A list
//! type keeps the type of its items, and a union takes the kind its members
//! share, or stays a string when they differ.

use xsd_fast_core::schema::{ContentModel, SimpleTypeDef, SimpleTypeKind, XsdType};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:simpleType name="Code"><xs:restriction base="xs:int">
    <xs:minInclusive value="1"/><xs:maxInclusive value="9"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="SmallCode"><xs:restriction base="t:Code">
    <xs:maxInclusive value="5"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="Flag"><xs:restriction base="t:SmallCode"/></xs:simpleType>
  <xs:simpleType name="Letters"><xs:restriction base="xs:string">
    <xs:enumeration value="A"/><xs:enumeration value="B"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="OneLetter"><xs:restriction base="t:Letters">
    <xs:maxLength value="1"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="Ints"><xs:list itemType="xs:int"/></xs:simpleType>
  <xs:simpleType name="Codes"><xs:list itemType="t:SmallCode"/></xs:simpleType>
  <xs:simpleType name="TwoCodes"><xs:restriction base="t:Codes">
    <xs:maxLength value="2"/></xs:restriction></xs:simpleType>
  <xs:simpleType name="Inline"><xs:list><xs:simpleType>
    <xs:restriction base="xs:date"/></xs:simpleType></xs:list></xs:simpleType>
  <xs:simpleType name="IntOrDate"><xs:union memberTypes="xs:int xs:date"/></xs:simpleType>
  <xs:simpleType name="IntOrCode"><xs:union memberTypes="xs:int t:Flag"/></xs:simpleType>
  <xs:simpleType name="IntOrInline"><xs:union memberTypes="xs:int">
    <xs:simpleType><xs:restriction base="t:Code"/></xs:simpleType></xs:union></xs:simpleType>
  <xs:simpleType name="When"><xs:restriction base="xs:date"/></xs:simpleType>
  <xs:simpleType name="LoopA"><xs:restriction base="t:LoopB"/></xs:simpleType>
  <xs:simpleType name="LoopB"><xs:restriction base="t:LoopA"/></xs:simpleType>
  <xs:simpleType name="NMTOKEN"><xs:restriction base="xs:int"/></xs:simpleType>
  <xs:element name="root">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="code" type="t:SmallCode"/>
        <xs:element name="flag" type="t:Flag" minOccurs="0"/>
        <xs:element name="letter" type="t:OneLetter" minOccurs="0"/>
        <xs:element name="ints" type="t:Ints" minOccurs="0"/>
        <xs:element name="codes" type="t:TwoCodes" minOccurs="0"/>
        <xs:element name="either" type="t:IntOrDate" minOccurs="0"/>
        <xs:element name="number" type="t:IntOrCode" minOccurs="0"/>
        <xs:element name="inline" minOccurs="0"><xs:simpleType>
          <xs:restriction base="t:SmallCode"/></xs:simpleType></xs:element>
        <xs:element name="text" minOccurs="0"><xs:complexType><xs:simpleContent>
          <xs:extension base="t:SmallCode"><xs:attribute name="on" type="t:When"/>
          </xs:extension></xs:simpleContent></xs:complexType></xs:element>
        <xs:element name="loop" type="t:LoopA" minOccurs="0"/>
      </xs:sequence>
      <xs:attribute name="n" type="t:SmallCode"/>
      <xs:attribute name="token" type="xs:NMTOKEN"/>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

fn load() -> SchemaSet {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads")
}

fn named<'a>(schema: &'a SchemaSet, name: &str) -> &'a SimpleTypeDef {
    &schema.model().simple_types[name]
}

fn messages(schema: &SchemaSet, xml: &str) -> Vec<String> {
    schema
        .validate(xml.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| format!("{} {}", finding.path, finding.message))
        .collect()
}

#[test]
fn a_chain_of_restrictions_takes_the_kind_and_facets_of_every_link() {
    let schema = load();
    for name in ["SmallCode", "Flag"] {
        let simple = named(&schema, name);
        assert_eq!(simple.kind, SimpleTypeKind::Int, "{name}");
        assert_eq!(simple.facets.min_inclusive.as_deref(), Some("1"), "{name}");
        // The nearer restriction wins.
        assert_eq!(simple.facets.max_inclusive.as_deref(), Some("5"), "{name}");
    }
    let letter = named(&schema, "OneLetter");
    assert_eq!(letter.kind, SimpleTypeKind::String);
    assert_eq!(letter.facets.enumerations.len(), 2);
    assert_eq!(letter.facets.max_length, Some(1));
}

#[test]
fn an_attribute_and_an_inline_restriction_take_the_chain_too() {
    let schema = load();
    let XsdType::Complex(root) = &schema.model().global_elements["root"].type_def else {
        panic!("root has an inline complex type");
    };
    let n = &root.attributes[0];
    assert_eq!(n.type_def.kind, SimpleTypeKind::Int);
    assert_eq!(n.type_def.facets.max_inclusive.as_deref(), Some("5"));
    let ContentModel::Sequence(particles) = &root.content else {
        panic!("root has a sequence");
    };
    let inline = particles
        .iter()
        .find_map(|particle| match &particle.element {
            xsd_fast_core::schema::ElementRef::Inline(decl) if decl.name.local == "inline" => {
                Some(decl)
            }
            _ => None,
        })
        .expect("inline element is declared");
    let XsdType::Simple(simple) = &inline.type_def else {
        panic!("inline element has a simple type");
    };
    assert_eq!(simple.kind, SimpleTypeKind::Int);
    assert_eq!(simple.facets.max_inclusive.as_deref(), Some("5"));
}

#[test]
fn a_built_in_name_is_never_taken_for_the_schemas_own_type() {
    // Passes on main too: guards the namespace check of the chain walk.
    let schema = load();
    let XsdType::Complex(root) = &schema.model().global_elements["root"].type_def else {
        panic!("root has an inline complex type");
    };
    assert_eq!(
        root.attributes[1].type_def.kind,
        SimpleTypeKind::Other("NMTOKEN".to_owned())
    );
}

#[test]
fn a_cycle_of_restrictions_loads_and_keeps_the_name() {
    let schema = load();
    assert!(matches!(
        &named(&schema, "LoopA").kind,
        SimpleTypeKind::Other(name) if name == "LoopB"
    ));
}

#[test]
fn a_list_keeps_the_type_of_its_items() {
    let schema = load();
    let ints = named(&schema, "Ints");
    assert_eq!(ints.kind, SimpleTypeKind::String);
    let item = ints.list_item.as_deref().expect("Ints is a list");
    assert_eq!(item.kind, SimpleTypeKind::Int);

    let codes = named(&schema, "TwoCodes");
    let item = codes.list_item.as_deref().expect("TwoCodes is a list");
    assert_eq!(item.kind, SimpleTypeKind::Int);
    assert_eq!(item.facets.max_inclusive.as_deref(), Some("5"));
    assert_eq!(codes.facets.max_length, Some(2));

    let inline = named(&schema, "Inline");
    let item = inline.list_item.as_deref().expect("Inline is a list");
    assert_eq!(item.kind, SimpleTypeKind::Date);
}

#[test]
fn a_union_takes_the_kind_its_members_share() {
    let schema = load();
    assert_eq!(named(&schema, "IntOrDate").kind, SimpleTypeKind::String);
    assert_eq!(named(&schema, "IntOrCode").kind, SimpleTypeKind::Int);
    assert_eq!(named(&schema, "IntOrInline").kind, SimpleTypeKind::Int);
    assert_eq!(named(&schema, "IntOrInline").union_members.len(), 2);
}

#[test]
fn simple_content_extending_an_own_simple_type_keeps_its_text_type() {
    let schema = load();
    let XsdType::Complex(root) = &schema.model().global_elements["root"].type_def else {
        panic!("root has an inline complex type");
    };
    let ContentModel::Sequence(particles) = &root.content else {
        panic!("root has a sequence");
    };
    let text = particles
        .iter()
        .find_map(|particle| match &particle.element {
            xsd_fast_core::schema::ElementRef::Inline(decl) if decl.name.local == "text" => {
                Some(decl)
            }
            _ => None,
        })
        .expect("text element is declared");
    let XsdType::Complex(text) = &text.type_def else {
        panic!("text has a complex type");
    };
    let ContentModel::SimpleContent(simple) = &text.content else {
        panic!("text has simple content");
    };
    assert_eq!(simple.kind, SimpleTypeKind::Int);
    assert_eq!(simple.facets.max_inclusive.as_deref(), Some("5"));
    assert_eq!(text.attributes[0].type_def.kind, SimpleTypeKind::Date);
}

#[test]
fn validation_applies_the_facets_of_the_chain() {
    let schema = load();
    let findings = messages(
        &schema,
        r#"<root xmlns="urn:t"><code>7</code><flag>0</flag><letter>C</letter>
        <inline>6</inline><text>8</text></root>"#,
    );
    assert!(
        findings.contains(&"/root/code value 7 exceeds maxInclusive 5".to_owned()),
        "{findings:?}"
    );
    assert!(
        findings.contains(&"/root/flag value 0 below minInclusive 1".to_owned()),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f.starts_with("/root/letter value \"C\" not in enumeration")),
        "{findings:?}"
    );
    assert!(
        findings.contains(&"/root/inline value 6 exceeds maxInclusive 5".to_owned()),
        "{findings:?}"
    );
    assert!(
        findings.contains(&"/root/text value 8 exceeds maxInclusive 5".to_owned()),
        "{findings:?}"
    );
}

#[test]
fn validation_counts_list_items_and_checks_each_one() {
    let schema = load();
    let findings = messages(
        &schema,
        r#"<root xmlns="urn:t"><code>1</code><ints>1 2 x</ints><codes>1 7 3</codes></root>"#,
    );
    assert_eq!(
        findings,
        [
            "/root/ints expected numeric value for Int, got \"x\"",
            "/root/codes value length 3 exceeds maxLength 2",
            "/root/codes value 7 exceeds maxInclusive 5",
        ],
    );
    // A list within its bounds is clean; its length is not counted in characters.
    assert!(messages(
        &schema,
        r#"<root xmlns="urn:t"><code>1</code><codes>1 2</codes></root>"#
    )
    .is_empty());
}

#[test]
fn a_union_value_is_not_held_to_one_members_facets() {
    let schema = load();
    let findings = messages(
        &schema,
        r#"<root xmlns="urn:t"><code>1</code><either>2020-01-01</either><number>70</number></root>"#,
    );
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn a_model_serialized_without_list_and_union_fields_loads() {
    let schema = load();
    let mut json = serde_json::to_value(schema.model()).expect("model serializes");
    let types = json["simple_types"]
        .as_object_mut()
        .expect("simple types serialize as a map");
    let mut stripped = 0;
    for simple in types.values_mut() {
        let object = simple.as_object_mut().expect("a simple type is an object");
        stripped += usize::from(object.remove("list_item").is_some());
        stripped += usize::from(object.remove("union_members").is_some());
    }
    // Four lists and three unions carry the fields; no other type does.
    assert_eq!(stripped, 7);
    let older: xsd_fast_core::schema::SchemaModel =
        serde_json::from_value(json).expect("older model loads");
    assert!(older.simple_types["Ints"].list_item.is_none());
    assert!(older.simple_types["IntOrDate"].union_members.is_empty());
    assert_eq!(older.simple_types["SmallCode"].kind, SimpleTypeKind::Int);
}

#[cfg(feature = "arrow")]
mod arrow {
    use super::load;
    use arrow_schema::{DataType, Field};
    use xsd_fast_core::arrow::ArrowOptions;

    fn data_type(name: &str) -> DataType {
        let schema = load()
            .to_arrow_schema("root", ArrowOptions::default())
            .expect("root derives");
        schema
            .field_with_name(name)
            .expect("column exists")
            .data_type()
            .clone()
    }

    #[test]
    fn columns_take_the_type_of_the_chain() {
        assert_eq!(data_type("code"), DataType::Int64);
        assert_eq!(data_type("flag"), DataType::Int64);
        assert_eq!(data_type("inline"), DataType::Int64);
        assert_eq!(data_type("@n"), DataType::Int64);
        assert_eq!(data_type("number"), DataType::Int64);
        assert_eq!(data_type("either"), DataType::Utf8);
        assert_eq!(data_type("letter"), DataType::Utf8);
        assert_eq!(data_type("@token"), DataType::Utf8);
    }

    #[test]
    fn a_list_column_is_a_list_of_its_items() {
        let item = |data_type| std::sync::Arc::new(Field::new("item", data_type, true));
        assert_eq!(data_type("ints"), DataType::List(item(DataType::Int64)));
        assert_eq!(data_type("codes"), DataType::List(item(DataType::Int64)));
    }

    #[test]
    fn simple_content_on_an_own_simple_type_has_a_text_column() {
        let DataType::Struct(fields) = data_type("text") else {
            panic!("text is a struct of its attribute and its text");
        };
        let names: Vec<_> = fields.iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["@on", "$text"]);
        assert_eq!(fields[1].data_type(), &DataType::Int64);
        assert_eq!(fields[0].data_type(), &DataType::Date32);
    }
}

#[test]
fn a_list_value_is_compared_with_its_white_space_collapsed() {
    const LISTS: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
      <xs:simpleType name="Ints"><xs:list itemType="xs:int"/></xs:simpleType>
      <xs:simpleType name="Pair"><xs:restriction base="Ints">
        <xs:enumeration value="1 2"/></xs:restriction></xs:simpleType>
      <xs:element name="p" type="Pair"/>
    </xs:schema>"#;
    let schema = SchemaSet::load_bytes(LISTS, ParseOptions::default()).expect("schema loads");
    let findings = |xml: &str| {
        schema
            .validate(xml.as_bytes(), ValidateOptions::default())
            .expect("validation runs")
            .len()
    };
    assert_eq!(findings("<p>1   2</p>"), 0);
    assert_eq!(findings("<p>\n 1\t2 </p>"), 0);
    assert_eq!(findings("<p>1 3</p>"), 1);
}
