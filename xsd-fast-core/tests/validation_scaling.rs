//! Validation time must not be schema size times instance size.
//!
//! Each closed element is checked against the particles it could be affected
//! by (its required children and the children it has), not against every
//! particle of its type, and each child is found by name, not by scanning.

use std::time::{Duration, Instant};

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// Optional children declared in the one wide type.
const DECLARED_CHILDREN: usize = 50_000;
/// Elements in the instance document.
const INSTANCE_ELEMENTS: usize = 20_000;
/// Far above what the indexed walk needs in an unoptimized build on a loaded
/// machine (well under a second), far below the product of the two sizes
/// (about a minute).
const BUDGET: Duration = Duration::from_secs(15);

fn wide_schema() -> String {
    let mut text = String::from(
        "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"r\">\
         <xs:complexType><xs:sequence><xs:element name=\"row\" type=\"Row\" minOccurs=\"0\" \
         maxOccurs=\"unbounded\"/></xs:sequence></xs:complexType></xs:element>\
         <xs:complexType name=\"Row\"><xs:sequence><xs:element name=\"must\" type=\"xs:int\"/>",
    );
    for index in 0..DECLARED_CHILDREN {
        text.push_str(&format!(
            "<xs:element name=\"c{index}\" type=\"xs:int\" minOccurs=\"0\"/>"
        ));
    }
    text.push_str("</xs:sequence></xs:complexType></xs:schema>");
    text
}

#[test]
fn a_wide_type_does_not_multiply_validation_time() {
    let schema =
        SchemaSet::load_bytes(wide_schema().as_bytes(), ParseOptions::default()).expect("loads");
    let mut document = String::from("<r>");
    let rows = INSTANCE_ELEMENTS / 4;
    for row in 0..rows {
        let last = DECLARED_CHILDREN - 1 - row;
        // Row 7 lacks its required child, repeats one and has a bad value.
        if row == 7 {
            document.push_str(&format!(
                "<row><c{last}>1</c{last}><c{last}>x</c{last}></row>"
            ));
        } else {
            document.push_str(&format!(
                "<row><must>1</must><c{row}>2</c{row}><c{last}>3</c{last}></row>"
            ));
        }
    }
    document.push_str("</r>");

    let started = Instant::now();
    let findings = schema
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs");
    let elapsed = started.elapsed();
    println!("validated {INSTANCE_ELEMENTS} elements against {DECLARED_CHILDREN} particles in {elapsed:?}");

    let messages: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
    let last = DECLARED_CHILDREN - 8;
    assert_eq!(
        messages,
        [
            "expected numeric value for Int, got \"x\"".to_string(),
            "required child <must> is missing".to_string(),
            format!("child <c{last}> appears 2 times, max is 1"),
        ]
    );
    assert!(elapsed < BUDGET, "validation took {elapsed:?}");
}
