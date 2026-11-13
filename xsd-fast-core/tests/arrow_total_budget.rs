//! Deriving every global element is bounded as one call.
//!
//! Each element has its own step budget, but a schema can declare thousands
//! of elements that each stay under it, and every result is kept. The call
//! as a whole is bounded too.
#![cfg(feature = "arrow")]

use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet};

/// `elements` global elements, each of one type with `columns` string children.
fn schema(elements: usize, columns: usize) -> SchemaSet {
    let mut text = String::from(
        "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:complexType name=\"T\"><xs:sequence>",
    );
    for column in 0..columns {
        text.push_str(&format!(
            "<xs:element name=\"c{column}\" type=\"xs:string\"/>"
        ));
    }
    text.push_str("</xs:sequence></xs:complexType>");
    for element in 0..elements {
        text.push_str(&format!("<xs:element name=\"e{element}\" type=\"T\"/>"));
    }
    text.push_str("</xs:schema>");
    SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).expect("schema parses")
}

#[test]
fn many_cheap_elements_cannot_add_up_without_bound() {
    // Each element is far below the per-element limit; together they ask for
    // 2,500,000 fields from a schema of about 120 KiB.
    let schema = schema(2_500, 1_000);
    let one = schema
        .to_arrow_schema("e2499", ArrowOptions::default())
        .expect("one element derives");
    assert_eq!(one.fields().len(), 1_000);

    let error = schema
        .try_all_arrow_schemas(ArrowOptions::default())
        .expect_err("the call as a whole is refused");
    let message = format!("{error:#}");
    assert!(
        message.starts_with("resource_limit: arrow_derivation_steps_total"),
        "{message}"
    );

    // The lenient form keeps what was derived inside the budget, in order,
    // and stops there.
    let derived = schema.all_arrow_schemas(ArrowOptions::default());
    assert!(
        !derived.is_empty() && derived.len() < 2_500,
        "{}",
        derived.len()
    );
    let fields: usize = derived.iter().map(|(_, s)| s.fields().len()).sum();
    assert!(fields <= 2_000_000, "{fields}");
    for (index, (name, _)) in derived.iter().enumerate() {
        assert_eq!(name, &format!("e{index}"));
    }
}

#[test]
fn a_schema_inside_the_budget_derives_every_element() {
    let schema = schema(50, 100);
    let strict = schema
        .try_all_arrow_schemas(ArrowOptions::default())
        .expect("derives");
    assert_eq!(strict.len(), 50);
    assert_eq!(schema.all_arrow_schemas(ArrowOptions::default()).len(), 50);
}
