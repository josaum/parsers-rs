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

/// `length` types of `width` elements each, every one extending the one
/// before it; `root` is of the last type.
fn wide_chain(length: usize, width: usize) -> String {
    let elements = |index: usize| -> String {
        (0..width)
            .map(|element| format!(r#"<xs:element name="e{index}_{element}" type="xs:string"/>"#))
            .collect()
    };
    let mut xsd = String::from(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">"#);
    xsd.push_str(&format!(
        r#"<xs:complexType name="C0"><xs:sequence>{}</xs:sequence></xs:complexType>"#,
        elements(0)
    ));
    for index in 1..length {
        xsd.push_str(&format!(
            r#"<xs:complexType name="C{index}"><xs:complexContent><xs:extension base="C{}"><xs:sequence>{}</xs:sequence></xs:extension></xs:complexContent></xs:complexType>"#,
            index - 1,
            elements(index)
        ));
    }
    xsd.push_str(&format!(
        r#"<xs:element name="root" type="C{}"/></xs:schema>"#,
        length - 1
    ));
    xsd
}

// The nearest sixteen bases are always folded, and their cost can refuse a
// schema. Bases further away are folded while the budget for them lasts and
// are left out after that, without an error: a chain is never the reason a
// schema that used to load is refused.
#[test]
fn a_chain_too_long_to_fold_whole_loads_with_its_nearest_bases() {
    for length in [700, 5000] {
        let schema =
            SchemaSet::load_bytes(wide_chain(length, 1).as_bytes(), ParseOptions::default())
                .unwrap_or_else(|error| panic!("{length} types: {error}"));
        let types = &schema.model().complex_types;
        assert_eq!(types.len(), length);
        // Every type has its own element and those of its sixteen nearest
        // bases, in order, at the end of what it inherits.
        for index in [16, length / 2, length - 1] {
            let names = particle_names(&types[&format!("C{index}")].content);
            let nearest: Vec<String> = (index - 16..=index).map(|i| format!("e{i}_0")).collect();
            assert!(names.len() >= 17, "C{index} has {}", names.len());
            assert_eq!(names[names.len() - 17..], nearest[..], "C{index}");
        }
        // An early type is folded whole.
        assert_eq!(particle_names(&types["C40"].content).len(), 41);
    }
}

// What the nearest sixteen bases cost is still charged in full.
#[test]
fn a_chain_whose_nearest_bases_cost_too_much_is_refused() {
    let error = SchemaSet::load_bytes(wide_chain(700, 50).as_bytes(), ParseOptions::default())
        .err()
        .expect("the schema is refused");
    assert!(error.to_string().contains("inherited_nodes"), "{error}");
}
