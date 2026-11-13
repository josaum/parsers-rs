//! A model serialized before `ComplexType` gained fields still loads.
//!
//! `derivation`, `prohibited_attributes` and `empty_in_source` were added to
//! `ComplexType` after models were first serialized. A model written without
//! them reads back with each at its default.

use xsd_fast_core::schema::{Derivation, SchemaModel};
use xsd_fast_core::{ParseOptions, SchemaSet};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="Base"><xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence>
    <xs:attribute name="id" type="xs:string"/><xs:attribute name="old" type="xs:int"/></xs:complexType>
  <xs:complexType name="Restricted"><xs:complexContent><xs:restriction base="Base">
    <xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence>
    <xs:attribute name="old" use="prohibited"/>
  </xs:restriction></xs:complexContent></xs:complexType>
  <xs:complexType name="Marker"/>
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="r" type="Restricted"/>
    <xs:element name="m" type="Marker"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

const ADDED_FIELDS: [&str; 3] = ["derivation", "prohibited_attributes", "empty_in_source"];

/// Removes the added fields from every object in `value`; returns how many.
fn strip_added_fields(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            let removed = ADDED_FIELDS
                .iter()
                .filter(|field| map.remove(**field).is_some())
                .count();
            removed + map.values_mut().map(strip_added_fields).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter_mut().map(strip_added_fields).sum(),
        _ => 0,
    }
}

#[test]
fn a_model_serialized_without_the_added_fields_loads_with_their_defaults() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let current = &schema.model().complex_types;
    assert_eq!(current["Restricted"].derivation, Derivation::Restriction);
    assert_eq!(current["Restricted"].prohibited_attributes, ["old"]);
    assert!(current["Marker"].empty_in_source);

    let mut json = serde_json::to_value(schema.model()).expect("model serializes");
    // Three named types and the inline type of `root`, three fields each.
    assert_eq!(strip_added_fields(&mut json), 12);
    let older: SchemaModel = serde_json::from_value(json).expect("older model loads");

    assert_eq!(older.complex_types.len(), current.len());
    for (name, complex) in &older.complex_types {
        assert_eq!(complex.derivation, Derivation::Extension, "{name}");
        assert!(complex.prohibited_attributes.is_empty(), "{name}");
        assert!(!complex.empty_in_source, "{name}");
        // What was serialized is unchanged.
        assert_eq!(complex.base, current[name].base, "{name}");
        assert_eq!(
            complex.attributes.len(),
            current[name].attributes.len(),
            "{name}"
        );
    }
}
