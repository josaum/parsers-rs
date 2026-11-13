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

// The recursion is unrolled once: the inline type gets its base, and the
// same extension inside that copy is left as written.
#[test]
fn recursion_is_unrolled_once() {
    let schema =
        SchemaSet::load_bytes(SELF_RECURSIVE, ParseOptions::default()).expect("schema loads");
    let recursive = &schema.model().complex_types["N"];
    assert_eq!(children(recursive), ["v", "child"]);
    let unrolled = child_type(recursive);
    assert_eq!(children(unrolled), ["v", "child", "x"]);
    assert_eq!(children(child_type(unrolled)), ["x"]);
}

// A type derived from the recursive one is unrolled as far as it is.
#[test]
fn a_type_derived_from_a_recursive_one_is_unrolled_alike() {
    let schema = SchemaSet::load_bytes(TWO_RECURSIVE_CHILDREN, ParseOptions::default())
        .expect("schema loads");
    let derived = &schema.model().complex_types["D"];
    assert_eq!(children(derived), ["v", "child", "other", "d"]);
    let unrolled = child_type(derived);
    assert_eq!(children(unrolled), ["v", "child", "other", "x"]);
    assert_eq!(children(child_type(unrolled)), ["x"]);
}

const TWO_RECURSIVE_CHILDREN: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="N"><xs:sequence><xs:element name="v" type="xs:string"/><xs:element name="child" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="x" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element><xs:element name="other" minOccurs="0"><xs:complexType><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="y" type="xs:string" minOccurs="0"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element></xs:sequence></xs:complexType><xs:complexType name="D"><xs:complexContent><xs:extension base="N"><xs:sequence><xs:element name="d" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType><xs:element name="root" type="N"/></xs:schema>"#;

// Unrolling this to any depth doubles the copies with every level. Unrolled
// once, it costs two copies.
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
