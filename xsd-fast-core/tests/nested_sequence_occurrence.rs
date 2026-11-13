//! A nested `xs:sequence` keeps its own occurrence.
//!
//! `<xs:sequence minOccurs="0">` inside another group is optional as a
//! whole: the element is valid without any of its children, and using one of
//! them makes the group's required children required.

use xsd_fast_core::schema::{ContentModel, ElementRef};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="order"><xs:complexType><xs:sequence>
    <xs:element name="id" type="xs:string"/>
    <xs:sequence minOccurs="0">
      <xs:element name="street" type="xs:string"/>
      <xs:element name="city" type="xs:string"/>
      <xs:element name="note" type="xs:string" minOccurs="0"/>
    </xs:sequence>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="plain"><xs:complexType><xs:sequence>
    <xs:element name="id" type="xs:string"/>
    <xs:sequence>
      <xs:element name="street" type="xs:string"/>
      <xs:element name="hint" type="xs:string" minOccurs="0"/>
    </xs:sequence>
    <xs:sequence>
      <xs:element name="extra" type="xs:string" minOccurs="0"/>
    </xs:sequence>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

fn messages(document: &str) -> Vec<String> {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

#[test]
fn an_absent_optional_sequence_is_valid() {
    assert_eq!(messages("<order><id>1</id></order>"), Vec::<String>::new());
}

#[test]
fn a_complete_optional_sequence_is_valid() {
    assert_eq!(
        messages("<order><id>1</id><street>s</street><city>c</city></order>"),
        Vec::<String>::new()
    );
}

#[test]
fn a_partially_present_optional_sequence_is_an_error() {
    assert_eq!(
        messages("<order><id>1</id><street>s</street></order>"),
        ["required child <city> is missing"]
    );
    // An optional member alone starts the group as well.
    assert_eq!(
        messages("<order><id>1</id><note>n</note></order>"),
        [
            "required child <street> is missing",
            "required child <city> is missing"
        ]
    );
}

#[test]
fn a_sequence_that_occurs_once_is_still_part_of_its_parent() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let model = schema.model();
    let particles = |name: &str| {
        let xsd_fast_core::XsdType::Complex(complex) = &model.global_elements[name].type_def else {
            panic!("expected a complex type");
        };
        let ContentModel::Sequence(particles) = &complex.content else {
            panic!("expected a sequence");
        };
        particles
    };
    // Occurring exactly once, the nested sequences add nothing of their own.
    assert_eq!(particles("plain").len(), 4);
    assert!(particles("plain")
        .iter()
        .all(|particle| matches!(particle.element, ElementRef::Inline(_))));
    assert_eq!(
        messages("<plain><id>1</id></plain>"),
        ["required child <street> is missing"]
    );
    // The optional one is kept as a group with its occurrence.
    let order = particles("order");
    assert_eq!(order.len(), 2);
    assert_eq!(order[1].occurrence.min, 0);
    assert!(matches!(
        &order[1].element,
        ElementRef::Group(group) if matches!(**group, ContentModel::Sequence(_))
    ));
}
