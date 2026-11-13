//! Arrow columns of elements whose complex type declares nothing.
//!
//! An element typed by a complex type with no particles and no attributes is
//! a marker: all it can say is that it is there. It keeps a `Utf8` column,
//! whether the type is written as a self-closing tag or opened and closed,
//! so the marker is not lost and its parent keeps its shape.
#![cfg(feature = "arrow")]

use arrow_schema::{DataType, Field};
use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet};

const SELF_CLOSING: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="Emp"/><xs:simpleType name="St"/><xs:element name="root"><xs:complexType><xs:sequence><xs:element name="p" type="Emp"/><xs:element name="s" type="St"/></xs:sequence></xs:complexType></xs:element></xs:schema>"#;

const OPEN_AND_CLOSED: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="Emp"></xs:complexType><xs:complexType name="EmpA"><xs:attribute name="k" type="xs:string"/></xs:complexType><xs:element name="root"><xs:complexType><xs:sequence><xs:element name="p" type="Emp"/><xs:element name="pa" type="EmpA"/><xs:element name="grp"><xs:complexType><xs:choice><xs:element name="none" type="Emp"/><xs:element name="some"><xs:complexType><xs:sequence><xs:element name="x" type="xs:string"/></xs:sequence></xs:complexType></xs:element></xs:choice></xs:complexType></xs:element></xs:sequence></xs:complexType></xs:element></xs:schema>"#;

const REPEATED_PARENT: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="Emp"/>
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="item" maxOccurs="unbounded"><xs:complexType><xs:sequence>
      <xs:element name="name" type="xs:string"/>
      <xs:element name="flag" type="Emp" minOccurs="0"/>
    </xs:sequence></xs:complexType></xs:element>
    <xs:element name="inline" minOccurs="0"><xs:complexType/></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

/// The columns of `root`: name, type and nullability.
fn columns(schema: &[u8]) -> Vec<(String, DataType, bool)> {
    let schema = SchemaSet::load_bytes(schema, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("schema derives");
    arrow
        .fields()
        .iter()
        .map(|field| {
            (
                field.name().clone(),
                field.data_type().clone(),
                field.is_nullable(),
            )
        })
        .collect()
}

fn column(schema: &[u8], name: &str) -> (DataType, bool) {
    columns(schema)
        .into_iter()
        .find(|(column, _, _)| column == name)
        .map(|(_, data_type, nullable)| (data_type, nullable))
        .unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn a_self_closing_empty_type_keeps_its_column() {
    assert_eq!(
        columns(SELF_CLOSING),
        [
            ("p".to_string(), DataType::Utf8, false),
            ("s".to_string(), DataType::Utf8, false),
        ]
    );
}

#[test]
fn an_open_and_closed_empty_type_has_the_same_column() {
    assert_eq!(column(OPEN_AND_CLOSED, "p"), (DataType::Utf8, false));
    // A type with an attribute and no content is not a marker: its one
    // column is the attribute.
    assert_eq!(column(OPEN_AND_CLOSED, "pa"), (DataType::Utf8, false));
}

#[test]
fn a_parent_keeps_its_shape_beside_an_empty_child() {
    // Both branches of the choice are columns, so the parent is a struct.
    let (group, _) = column(OPEN_AND_CLOSED, "grp");
    let DataType::Struct(branches) = group else {
        panic!("expected a struct, got {group:?}");
    };
    let branches: Vec<_> = branches
        .iter()
        .map(|field| field.as_ref().clone())
        .collect();
    assert_eq!(
        branches,
        [
            Field::new("none", DataType::Utf8, false),
            Field::new("some", DataType::Utf8, false),
        ]
    );

    // A repeated parent stays a list of structs, not a list of its one
    // remaining child.
    let (items, _) = column(REPEATED_PARENT, "item");
    let DataType::List(item) = items else {
        panic!("expected a list, got {items:?}");
    };
    let DataType::Struct(members) = item.data_type() else {
        panic!("expected a list of structs, got {item:?}");
    };
    let members: Vec<_> = members.iter().map(|field| field.as_ref().clone()).collect();
    assert_eq!(
        members,
        [
            Field::new("name", DataType::Utf8, false),
            Field::new("flag", DataType::Utf8, true),
        ]
    );

    // An inline empty type is a marker as well.
    assert_eq!(column(REPEATED_PARENT, "inline"), (DataType::Utf8, true));
}

// The column is there at every flattening limit, as it is for an element of
// a type that is not declared.
#[test]
fn the_marker_column_does_not_depend_on_the_flattening_limit() {
    let schema = SchemaSet::load_bytes(SELF_CLOSING, ParseOptions::default()).expect("loads");
    for max_depth in 0..4 {
        let options = ArrowOptions {
            max_depth,
            ..ArrowOptions::default()
        };
        let arrow = schema
            .to_arrow_schema("root", options)
            .expect("schema derives");
        let names: Vec<_> = arrow.fields().iter().map(|field| field.name()).collect();
        assert_eq!(names, ["p", "s"], "max_depth {max_depth}");
    }
}
