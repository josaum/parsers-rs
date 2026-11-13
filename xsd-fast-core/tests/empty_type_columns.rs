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

// A type is a marker only when nothing is written in it but an annotation.
// The parser does not model attribute references, attribute groups, group
// references or wildcards, and it cannot fill in a derivation from a base
// it does not know: a type made of those has no particles and no attributes
// in the model, and is not empty. It derives what it always did, nothing.
const NOT_EMPTY_IN_SOURCE: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
<xs:attributeGroup name="AG"><xs:attribute name="g" type="xs:string"/></xs:attributeGroup>
<xs:attribute name="ga" type="xs:string"/>
<xs:group name="G"><xs:sequence><xs:element name="g1" type="xs:string"/></xs:sequence></xs:group>
<xs:complexType name="Annotated"><xs:annotation><xs:documentation>nothing</xs:documentation></xs:annotation></xs:complexType>
<xs:complexType name="AttributeRef"><xs:attribute ref="ga"/></xs:complexType>
<xs:complexType name="AttributeGroupRef"><xs:attributeGroup ref="AG"/></xs:complexType>
<xs:complexType name="GroupRef"><xs:group ref="G"/></xs:complexType>
<xs:complexType name="AnyElement"><xs:sequence><xs:any minOccurs="0"/></xs:sequence></xs:complexType>
<xs:complexType name="AnyAttribute"><xs:anyAttribute/></xs:complexType>
<xs:complexType name="EmptySequence"><xs:sequence/></xs:complexType>
<xs:complexType name="EmptyChoice"><xs:choice></xs:choice></xs:complexType>
<xs:complexType name="EmptyAll"><xs:all/></xs:complexType>
<xs:complexType name="UnknownExtension"><xs:complexContent><xs:extension base="Missing"/></xs:complexContent></xs:complexType>
<xs:complexType name="UnknownRestriction"><xs:complexContent><xs:restriction base="Missing"/></xs:complexContent></xs:complexType>
<xs:complexType name="UnknownSimpleExtension"><xs:simpleContent><xs:extension base="Missing"/></xs:simpleContent></xs:complexType>
<xs:complexType name="UnknownSimpleRestriction"><xs:simpleContent><xs:restriction base="Missing"/></xs:simpleContent></xs:complexType>
<xs:complexType name="AnyTypeRestriction"><xs:complexContent><xs:restriction base="xs:anyType"/></xs:complexContent></xs:complexType>
<xs:complexType name="Mixed" mixed="true"/>
<xs:element name="root"><xs:complexType><xs:sequence>
<xs:element name="anchor" type="xs:string"/>
<xs:element name="annotated" type="Annotated"/>
<xs:element name="attribute_ref" type="AttributeRef"/>
<xs:element name="attribute_group_ref" type="AttributeGroupRef"/>
<xs:element name="group_ref" type="GroupRef"/>
<xs:element name="any_element" type="AnyElement"/>
<xs:element name="any_attribute" type="AnyAttribute"/>
<xs:element name="empty_sequence" type="EmptySequence"/>
<xs:element name="empty_choice" type="EmptyChoice"/>
<xs:element name="empty_all" type="EmptyAll"/>
<xs:element name="unknown_extension" type="UnknownExtension"/>
<xs:element name="unknown_restriction" type="UnknownRestriction"/>
<xs:element name="unknown_simple_extension" type="UnknownSimpleExtension"/>
<xs:element name="unknown_simple_restriction" type="UnknownSimpleRestriction"/>
<xs:element name="any_type_restriction" type="AnyTypeRestriction"/>
<xs:element name="mixed" type="Mixed"/>
<xs:element name="inline_annotated"><xs:complexType><xs:annotation/></xs:complexType></xs:element>
<xs:element name="inline_any_attribute"><xs:complexType><xs:anyAttribute/></xs:complexType></xs:element>
<xs:element name="inline_open"><xs:complexType></xs:complexType></xs:element>
</xs:sequence></xs:complexType></xs:element></xs:schema>"#;

#[test]
fn a_type_holding_a_construct_that_is_not_modelled_is_not_a_marker() {
    let names: Vec<String> = columns(NOT_EMPTY_IN_SOURCE)
        .into_iter()
        .map(|(name, data_type, _)| {
            assert_eq!(data_type, DataType::Utf8, "{name}");
            name
        })
        .collect();
    assert_eq!(
        names,
        [
            "anchor",
            "annotated",
            "mixed",
            "inline_annotated",
            "inline_open"
        ]
    );
}

// One construct at a time, so a failure names it.
#[test]
fn each_construct_that_is_not_modelled_leaves_no_marker() {
    for construct in [
        r#"<xs:attribute ref="ga"/>"#,
        r#"<xs:attributeGroup ref="AG"/>"#,
        r#"<xs:group ref="G"/>"#,
        r#"<xs:sequence><xs:any minOccurs="0"/></xs:sequence>"#,
        r#"<xs:any/>"#,
        r#"<xs:anyAttribute/>"#,
        r#"<xs:sequence/>"#,
        r#"<xs:choice></xs:choice>"#,
        r#"<xs:all/>"#,
        r#"<xs:complexContent><xs:extension base="Missing"/></xs:complexContent>"#,
        r#"<xs:complexContent><xs:restriction base="Missing"/></xs:complexContent>"#,
        r#"<xs:complexContent><xs:restriction base="xs:anyType"/></xs:complexContent>"#,
        r#"<xs:simpleContent><xs:extension base="Missing"/></xs:simpleContent>"#,
        r#"<xs:simpleContent><xs:restriction base="Missing"/></xs:simpleContent>"#,
    ] {
        let schema = format!(
            r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="T">{construct}</xs:complexType><xs:element name="root"><xs:complexType><xs:sequence><xs:element name="anchor" type="xs:string"/><xs:element name="named" type="T"/><xs:element name="inline"><xs:complexType>{construct}</xs:complexType></xs:element></xs:sequence></xs:complexType></xs:element></xs:schema>"#
        );
        let names: Vec<String> = columns(schema.as_bytes())
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(names, ["anchor"], "{construct}");
    }
}
