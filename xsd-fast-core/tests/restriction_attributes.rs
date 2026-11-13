//! Attributes of a `complexContent` restriction.
//!
//! A restriction states its particles again but not its attributes: those of
//! the base stay, with the base's type, unless the restriction prohibits
//! them. A type extending the restriction inherits that result.

use xsd_fast_core::schema::{ComplexType, ContentModel, ElementRef};
use xsd_fast_core::{ParseOptions, SchemaSet, SimpleTypeKind};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
<xs:complexType name="Base"><xs:sequence><xs:element name="a" type="xs:string"/><xs:element name="b" type="xs:string" minOccurs="0"/><xs:element name="c" type="xs:string" minOccurs="0"/></xs:sequence>
<xs:attribute name="id" type="xs:string" use="required"/><xs:attribute name="lang" type="xs:string"/><xs:attribute name="old" type="xs:int"/></xs:complexType>
<xs:complexType name="R"><xs:complexContent><xs:restriction base="Base"><xs:sequence><xs:element name="a" type="xs:string"/><xs:element name="b" type="xs:string"/></xs:sequence><xs:attribute name="old" use="prohibited"/></xs:restriction></xs:complexContent></xs:complexType>
<xs:complexType name="RA"><xs:complexContent><xs:restriction base="Base"><xs:attribute name="lang" type="xs:string" use="required"/></xs:restriction></xs:complexContent></xs:complexType>
<xs:complexType name="E"><xs:complexContent><xs:extension base="R"><xs:sequence><xs:element name="z" type="xs:string"/></xs:sequence><xs:attribute name="extra" type="xs:string"/></xs:extension></xs:complexContent></xs:complexType>
<xs:element name="r" type="R"/><xs:element name="ra" type="RA"/><xs:element name="e" type="E"/><xs:element name="base" type="Base"/></xs:schema>"#;

fn schema() -> SchemaSet {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads")
}

/// Name, kind and `required` of every attribute of a type, in order.
fn attributes(complex: &ComplexType) -> Vec<(&str, &SimpleTypeKind, bool)> {
    complex
        .attributes
        .iter()
        .map(|attribute| {
            (
                attribute.name.as_str(),
                &attribute.type_def.kind,
                attribute.required,
            )
        })
        .collect()
}

/// Names of the elements a type declares, in order.
fn children(complex: &ComplexType) -> Vec<&str> {
    match &complex.content {
        ContentModel::Sequence(particles) => particles
            .iter()
            .map(|particle| match &particle.element {
                ElementRef::Inline(decl) => decl.name.local.as_str(),
                other => panic!("expected an inline element, got {other:?}"),
            })
            .collect(),
        ContentModel::Empty => Vec::new(),
        other => panic!("expected a sequence, got {other:?}"),
    }
}

#[test]
fn a_restriction_inherits_the_attributes_it_does_not_prohibit() {
    let schema = schema();
    let restriction = &schema.model().complex_types["R"];
    assert_eq!(
        attributes(restriction),
        [
            ("id", &SimpleTypeKind::String, true),
            ("lang", &SimpleTypeKind::String, false),
        ]
    );
    // The particles are the ones the restriction wrote: `c` is gone and `b`
    // is as the restriction declares it.
    assert_eq!(children(restriction), ["a", "b"]);
}

#[test]
fn a_restated_attribute_keeps_the_type_of_the_base() {
    let schema = schema();
    let restriction = &schema.model().complex_types["RA"];
    assert_eq!(
        attributes(restriction),
        [
            ("id", &SimpleTypeKind::String, true),
            ("lang", &SimpleTypeKind::String, true),
            ("old", &SimpleTypeKind::Int, false),
        ]
    );
    assert_eq!(children(restriction), Vec::<&str>::new());
    // The base itself is as it was written.
    assert_eq!(
        attributes(&schema.model().complex_types["Base"]),
        [
            ("id", &SimpleTypeKind::String, true),
            ("lang", &SimpleTypeKind::String, false),
            ("old", &SimpleTypeKind::Int, false),
        ]
    );
}

#[test]
fn an_extension_of_a_restriction_inherits_the_restricted_type() {
    let schema = schema();
    let extension = &schema.model().complex_types["E"];
    assert_eq!(
        attributes(extension),
        [
            ("id", &SimpleTypeKind::String, true),
            ("lang", &SimpleTypeKind::String, false),
            ("extra", &SimpleTypeKind::String, false),
        ]
    );
    assert_eq!(children(extension), ["a", "b", "z"]);
}

#[cfg(feature = "arrow")]
mod arrow {
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::ArrowOptions;

    /// Name, type and nullability of the columns of one global element.
    fn columns(element: &str) -> Vec<(String, DataType, bool)> {
        let arrow = super::schema()
            .to_arrow_schema(element, ArrowOptions::default())
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

    fn column(name: &str, data_type: DataType, nullable: bool) -> (String, DataType, bool) {
        (name.to_string(), data_type, nullable)
    }

    #[test]
    fn restriction_columns_hold_the_inherited_attributes() {
        assert_eq!(
            columns("r"),
            [
                column("@id", DataType::Utf8, false),
                column("@lang", DataType::Utf8, true),
                column("a", DataType::Utf8, false),
                column("b", DataType::Utf8, false),
            ]
        );
        assert_eq!(
            columns("ra"),
            [
                column("@id", DataType::Utf8, false),
                column("@lang", DataType::Utf8, false),
                column("@old", DataType::Int64, true),
            ]
        );
        assert_eq!(
            columns("e"),
            [
                column("@id", DataType::Utf8, false),
                column("@lang", DataType::Utf8, true),
                column("@extra", DataType::Utf8, true),
                column("a", DataType::Utf8, false),
                column("b", DataType::Utf8, false),
                column("z", DataType::Utf8, false),
            ]
        );
    }
}
