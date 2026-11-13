//! A type whose inline child extends the type itself.
//!
//! Recursive types are legal XSD. Folding the base into the inline extension
//! copies the base, which holds the same inline extension again, so the fold
//! has to stop by itself instead of running into a resource limit.

use xsd_fast_core::schema::{ComplexType, ContentModel, ElementRef, XsdType};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SELF_RECURSIVE: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="N"><xs:sequence><xs:element name="v" type="xs:string"/><xs:element name="child" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="x" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:element name="root" type="N"/></xs:schema>"#;

const MUTUALLY_RECURSIVE: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="A"><xs:sequence>
    <xs:element name="a" type="xs:string"/>
    <xs:element name="toB" minOccurs="0"><xs:complexType><xs:complexContent>
      <xs:extension base="B"><xs:sequence><xs:element name="xa" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension>
    </xs:complexContent></xs:complexType></xs:element>
  </xs:sequence></xs:complexType>
  <xs:complexType name="B"><xs:sequence>
    <xs:element name="b" type="xs:string"/>
    <xs:element name="toA" minOccurs="0"><xs:complexType><xs:complexContent>
      <xs:extension base="A"><xs:sequence><xs:element name="xb" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension>
    </xs:complexContent></xs:complexType></xs:element>
  </xs:sequence></xs:complexType>
  <xs:element name="root" type="A"/>
</xs:schema>"#;

#[test]
fn a_type_whose_inline_child_extends_it_loads_and_validates() {
    let schema =
        SchemaSet::load_bytes(SELF_RECURSIVE, ParseOptions::default()).expect("schema loads");
    let findings = schema
        .validate(
            b"<root><v/><child><v/><child><v/></child></child></root>",
            ValidateOptions::default(),
        )
        .expect("validation runs");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn mutually_recursive_inline_extensions_load_and_validate() {
    let schema =
        SchemaSet::load_bytes(MUTUALLY_RECURSIVE, ParseOptions::default()).expect("schema loads");
    let findings = schema
        .validate(
            b"<root><a/><toB><b/><toA><a/><toB><b/><xa/></toB><xb/></toA><xa/></toB></root>",
            ValidateOptions::default(),
        )
        .expect("validation runs");
    assert!(findings.is_empty(), "{findings:?}");
}

/// Names of the elements a complex type declares, in order.
fn children(complex: &ComplexType) -> Vec<&str> {
    let ContentModel::Sequence(particles) = &complex.content else {
        panic!("expected a sequence, got {:?}", complex.content);
    };
    particles
        .iter()
        .map(|particle| match &particle.element {
            ElementRef::Inline(decl) => decl.name.local.as_str(),
            other => panic!("expected an inline element, got {other:?}"),
        })
        .collect()
}

/// The inline type of the `child` element of `complex`.
fn child_type(complex: &ComplexType) -> &ComplexType {
    let ContentModel::Sequence(particles) = &complex.content else {
        panic!("expected a sequence, got {:?}", complex.content);
    };
    particles
        .iter()
        .find_map(|particle| match &particle.element {
            ElementRef::Inline(decl) if decl.name.local == "child" => match &decl.type_def {
                XsdType::Complex(inner) => Some(inner),
                _ => None,
            },
            _ => None,
        })
        .expect("a child element with an inline type")
}

// The recursion is unrolled as deep as folding ever went, and then ends at
// an inline type left as written.
#[test]
fn recursion_is_unrolled_fifteen_levels_and_then_ends() {
    let schema =
        SchemaSet::load_bytes(SELF_RECURSIVE, ParseOptions::default()).expect("schema loads");
    let mut complex = &schema.model().complex_types["N"];
    assert_eq!(children(complex), ["v", "child"]);
    for level in 1..=15 {
        complex = child_type(complex);
        assert_eq!(children(complex), ["v", "child", "x"], "level {level}");
    }
    assert_eq!(children(child_type(complex)), ["x"]);
}

// Every unrolled level requires what the base requires.
#[test]
fn an_unrolled_level_requires_the_inherited_child() {
    let schema =
        SchemaSet::load_bytes(SELF_RECURSIVE, ParseOptions::default()).expect("schema loads");
    for (document, path) in [
        (
            &b"<root><v/><child><v/><child><child><v/></child></child></child></root>"[..],
            "/root/child/child",
        ),
        (
            &b"<root><v/><child><child><v/></child></child></root>"[..],
            "/root/child",
        ),
    ] {
        let findings = schema
            .validate(document, ValidateOptions::default())
            .expect("validation runs");
        let findings: Vec<_> = findings
            .iter()
            .map(|finding| (finding.path.as_str(), finding.message.as_str()))
            .collect();
        assert_eq!(findings, [(path, "required child <v> is missing")]);
    }
}

/// `count` named types, each with an inline child extending the next.
fn run_of_types(count: usize) -> String {
    let mut text = String::from(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">"#);
    for index in 0..count {
        text.push_str(&format!(
            r#"<xs:complexType name="M{index}"><xs:sequence><xs:element name="e{index}" type="xs:string"/>"#
        ));
        if index + 1 < count {
            text.push_str(&format!(
                r#"<xs:element name="c{index}" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="M{}"><xs:sequence></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element>"#,
                index + 1
            ));
        }
        text.push_str("</xs:sequence></xs:complexType>");
    }
    text.push_str(r#"<xs:element name="root" type="M0"/></xs:schema>"#);
    text
}

// Nothing here is recursive, but every type nests as deep as the run is
// long. What is too deep, or more than the budget for it, is left as
// written; the schema loads.
#[test]
fn a_long_run_of_types_each_extending_the_next_inline_loads() {
    for count in [300, 600] {
        let schema = SchemaSet::load_bytes(run_of_types(count).as_bytes(), ParseOptions::default())
            .unwrap_or_else(|error| panic!("{count} types: {error}"));
        assert_eq!(schema.model().complex_types.len(), count);
        // The first levels are folded as they always were.
        let first = &schema.model().complex_types["M0"];
        assert_eq!(children(first), ["e0", "c0"]);
    }
}

/// One type with `count` inline children that each extend it.
fn fan(count: usize) -> String {
    let mut text = String::from(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="N"><xs:sequence><xs:element name="v" type="xs:string"/>"#,
    );
    for index in 0..count {
        text.push_str(&format!(
            r#"<xs:element name="k{index}" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="x{index}" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element>"#
        ));
    }
    text.push_str(
        r#"</xs:sequence></xs:complexType><xs:element name="root" type="N"/></xs:schema>"#,
    );
    text
}

// Unrolling these in full takes `count` to the fifteenth copies. The
// unrolling stops when the type has spent its allowance, without an error.
#[test]
fn a_type_with_many_recursive_children_loads() {
    for count in [2, 40] {
        let schema = SchemaSet::load_bytes(fan(count).as_bytes(), ParseOptions::default())
            .unwrap_or_else(|error| panic!("{count} children: {error}"));
        let recursive = &schema.model().complex_types["N"];
        assert_eq!(children(recursive).len(), count + 1);
    }
}

#[cfg(feature = "arrow")]
mod arrow {
    use arrow_schema::{DataType, Field};
    use xsd_fast_core::arrow::ArrowOptions;
    use xsd_fast_core::{ParseOptions, SchemaSet};

    const MUTUALLY_RECURSIVE_BARE: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="A"><xs:sequence><xs:element name="a" type="xs:string"/><xs:element name="toB" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="B"><xs:sequence></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:complexType name="B"><xs:sequence><xs:element name="b" type="xs:string"/><xs:element name="toA" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="A"><xs:sequence></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:element name="root" type="A"/></xs:schema>"#;

    fn paths(field: &Field) -> usize {
        1 + match field.data_type() {
            DataType::Struct(fields) => fields.iter().map(|field| paths(field)).sum(),
            DataType::List(item) => paths(item),
            _ => 0,
        }
    }

    /// Number of column paths of `root`, nested ones included.
    fn column_paths(schema: &[u8]) -> usize {
        let schema = SchemaSet::load_bytes(schema, ParseOptions::default()).expect("schema loads");
        let arrow = schema
            .to_arrow_schema("root", ArrowOptions::default())
            .expect("schema derives");
        arrow.fields().iter().map(|field| paths(field)).sum()
    }

    // The counts are those of the release before bases were followed to any
    // depth: the flattening limit, not the fold, is what ends the columns.
    #[test]
    fn recursive_types_keep_their_columns_to_the_flattening_limit() {
        assert_eq!(column_paths(super::SELF_RECURSIVE), 26);
        assert_eq!(column_paths(MUTUALLY_RECURSIVE_BARE), 18);
    }
}

const TWO_RECURSIVE_CHILDREN: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="N"><xs:sequence><xs:element name="v" type="xs:string"/><xs:element name="child" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="x" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element><xs:element name="other" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="y" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:complexType name="D"><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="d" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType><xs:element name="root" type="N"/></xs:schema>"#;

// Two recursive children, and a type derived from the recursive one.
#[test]
fn a_type_with_two_recursive_children_loads() {
    let schema = SchemaSet::load_bytes(TWO_RECURSIVE_CHILDREN, ParseOptions::default())
        .expect("schema loads");
    let findings = schema
        .validate(
            b"<root><v/><child><v/><other><v/><y/></other><x/></child><other><v/></other></root>",
            ValidateOptions::default(),
        )
        .expect("validation runs");
    assert!(findings.is_empty(), "{findings:?}");
}
