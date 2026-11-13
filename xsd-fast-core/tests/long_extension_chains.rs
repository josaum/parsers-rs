//! Extension chains longer than sixteen types.
//!
//! Every ancestor's particles and attributes reach the derived type, however
//! long the chain is, and an inline type nested deep in a content model is
//! folded like any other.

use xsd_fast_core::schema::{ContentModel, ElementRef, XsdType};
use xsd_fast_core::{ParseOptions, SchemaSet};

/// A schema where `T0` is the root type and each `T{i}` extends `T{i-1}`,
/// adding the element `e{i}`; `T0` also declares the attribute `a0`.
fn chain(length: usize) -> String {
    let mut xsd = String::from(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">"#);
    xsd.push_str(
        r#"<xs:complexType name="T0"><xs:sequence><xs:element name="e0" type="xs:string"/></xs:sequence><xs:attribute name="a0" type="xs:string"/></xs:complexType>"#,
    );
    for i in 1..length {
        xsd.push_str(&format!(
            r#"<xs:complexType name="T{i}"><xs:complexContent><xs:extension base="T{prev}"><xs:sequence><xs:element name="e{i}" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType>"#,
            prev = i - 1
        ));
    }
    xsd.push_str("</xs:schema>");
    xsd
}

fn particle_names(content: &ContentModel) -> Vec<String> {
    let ContentModel::Sequence(particles) = content else {
        panic!("expected a sequence");
    };
    particles
        .iter()
        .map(|particle| match &particle.element {
            ElementRef::Inline(decl) => decl.name.local.clone(),
            other => panic!("unexpected particle {other:?}"),
        })
        .collect()
}

#[test]
fn a_chain_of_forty_types_keeps_every_ancestor() {
    let schema =
        SchemaSet::load_bytes(chain(40).as_bytes(), ParseOptions::default()).expect("loads");
    let last = schema.model().complex_types.get("T39").expect("T39");
    let expected: Vec<String> = (0..40).map(|i| format!("e{i}")).collect();
    assert_eq!(particle_names(&last.content), expected);
    // The root type's attribute travels the whole chain too.
    assert_eq!(last.attributes[0].name, "a0");
}

#[test]
fn a_chain_of_five_hundred_types_loads_whole() {
    let schema =
        SchemaSet::load_bytes(chain(500).as_bytes(), ParseOptions::default()).expect("loads");
    let last = schema.model().complex_types.get("T499").expect("T499");
    assert_eq!(particle_names(&last.content).len(), 500);
}

#[test]
fn an_inline_extension_nested_deep_is_folded() {
    // Nine nested elements put the innermost inline type past the old cap.
    let mut xsd = String::from(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:complexType name="Base"><xs:sequence><xs:element name="inherited" type="xs:string"/></xs:sequence></xs:complexType><xs:element name="root">"#,
    );
    let levels = 9;
    for i in 0..levels {
        xsd.push_str(&format!(
            r#"<xs:complexType><xs:sequence><xs:element name="n{i}">"#
        ));
    }
    xsd.push_str(r#"<xs:complexType><xs:complexContent><xs:extension base="Base"><xs:sequence><xs:element name="own" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType>"#);
    for _ in 0..levels {
        xsd.push_str("</xs:element></xs:sequence></xs:complexType>");
    }
    xsd.push_str("</xs:element></xs:schema>");
    let schema = SchemaSet::load_bytes(xsd.as_bytes(), ParseOptions::default()).expect("loads");
    let mut type_def = &schema
        .model()
        .global_elements
        .get("root")
        .expect("root")
        .type_def;
    for _ in 0..levels {
        let XsdType::Complex(complex) = type_def else {
            panic!("expected an inline complex type");
        };
        let ContentModel::Sequence(particles) = &complex.content else {
            panic!("expected a sequence");
        };
        let ElementRef::Inline(decl) = &particles[0].element else {
            panic!("expected an inline element");
        };
        type_def = &decl.type_def;
    }
    let XsdType::Complex(innermost) = type_def else {
        panic!("expected the innermost complex type");
    };
    assert_eq!(particle_names(&innermost.content), ["inherited", "own"]);
}
