//! An inline extension nested inside another inline extension of the same base.
//!
//! The base is not recursive: it is extended twice, once inside the other.
//! Both extensions inherit its content.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="B"><xs:sequence><xs:element name="v" type="xs:string"/></xs:sequence><xs:attribute name="ba" type="xs:string"/></xs:complexType><xs:complexType name="T"><xs:sequence><xs:element name="outer"><xs:complexType><xs:complexContent><xs:extension base="B"><xs:sequence><xs:element name="inner"><xs:complexType><xs:complexContent><xs:extension base="B"><xs:sequence><xs:element name="w" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:element name="root" type="T"/></xs:schema>"#;

#[test]
fn the_inner_extension_requires_the_inherited_child() {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let findings = schema
        .validate(
            b"<root><outer><v/><inner><w/></inner></outer></root>",
            ValidateOptions::default(),
        )
        .expect("validation runs");
    let findings: Vec<_> = findings
        .iter()
        .map(|finding| (finding.path.as_str(), finding.message.as_str()))
        .collect();
    assert_eq!(
        findings,
        [("/root/outer/inner", "required child <v> is missing")]
    );
}

#[cfg(feature = "arrow")]
#[test]
fn the_inner_extension_has_the_inherited_columns() {
    use arrow_schema::{DataType, Field};
    use xsd_fast_core::arrow::ArrowOptions;

    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("root", ArrowOptions::default())
        .expect("schema derives");
    let inner = DataType::Struct(
        vec![
            Field::new("@ba", DataType::Utf8, true),
            Field::new("v", DataType::Utf8, false),
            Field::new("w", DataType::Utf8, false),
        ]
        .into(),
    );
    let outer = DataType::Struct(
        vec![
            Field::new("@ba", DataType::Utf8, true),
            Field::new("v", DataType::Utf8, false),
            Field::new("inner", inner, false),
        ]
        .into(),
    );
    assert_eq!(
        arrow
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect::<Vec<_>>(),
        [Field::new("outer", outer, false)]
    );
}
