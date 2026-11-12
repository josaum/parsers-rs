//! Regression tests for the fixed resource limits of the schema reader, the
//! validator and the Arrow derivation.
//!
//! Each limit is tested from both sides: a schema or document of a realistic
//! shape passes, and the hostile shape the limit exists for is still refused.

use xsd_fast_core::{ParseOptions, SchemaSet};

/// Wraps `body` in an `xs:schema` element with the `t` prefix bound.
fn schema_document(body: &str) -> Vec<u8> {
    format!(
        "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\" xmlns:t=\"urn:t\" \
         targetNamespace=\"urn:t\">{body}</xs:schema>"
    )
    .into_bytes()
}

fn load(bytes: &[u8]) -> anyhow::Result<SchemaSet> {
    SchemaSet::load_bytes(bytes, ParseOptions::default())
}

/// Text of an error that must be a refusal for exceeding a resource limit.
#[track_caller]
fn resource_limit_message<T>(result: anyhow::Result<T>) -> String {
    match result {
        Ok(_) => panic!("expected a resource_limit error, got a result"),
        Err(error) => {
            let message = format!("{error:#}");
            assert!(
                message.contains("resource_limit"),
                "expected a resource_limit error, got: {message}"
            );
            message
        }
    }
}

#[cfg(feature = "arrow")]
mod arrow {
    use super::{load, resource_limit_message, schema_document};
    use xsd_fast_core::arrow::ArrowOptions;

    /// A tree of named types: every type but the last holds `children`
    /// elements of the next type, and the last holds `children` strings.
    ///
    /// Nothing refers to itself, so the flattened schema is finite; it has
    /// `children ^ levels` leaf columns.
    fn named_type_tree(children: usize, levels: usize) -> Vec<u8> {
        let mut body = String::new();
        for level in 0..levels {
            body.push_str(&format!("<xs:complexType name=\"L{level}\"><xs:sequence>"));
            for child in 0..children {
                if level + 1 < levels {
                    body.push_str(&format!(
                        "<xs:element name=\"c{child}\" type=\"t:L{}\"/>",
                        level + 1
                    ));
                } else {
                    body.push_str(&format!(
                        "<xs:element name=\"c{child}\" type=\"xs:string\"/>"
                    ));
                }
            }
            body.push_str("</xs:sequence></xs:complexType>");
        }
        body.push_str("<xs:element name=\"big\" type=\"t:L0\"/>");
        body.push_str("<xs:element name=\"small\" type=\"xs:string\"/>");
        schema_document(&body)
    }

    /// A type with `children` optional elements of its own type.
    fn self_recursive_type(children: usize) -> Vec<u8> {
        let mut body = String::from("<xs:complexType name=\"T\"><xs:sequence>");
        for child in 0..children {
            body.push_str(&format!(
                "<xs:element name=\"c{child}\" type=\"t:T\" minOccurs=\"0\"/>"
            ));
        }
        body.push_str("</xs:sequence></xs:complexType>");
        body.push_str("<xs:element name=\"big\" type=\"t:T\"/>");
        body.push_str("<xs:element name=\"small\" type=\"xs:string\"/>");
        schema_document(&body)
    }

    #[test]
    fn a_large_finite_schema_derives() {
        // Six children over six levels is 46,656 leaf columns and more than
        // 100,000 walk steps, the size of the largest S1000D element.
        let schema = load(&named_type_tree(6, 6)).expect("a tree of named types is legal XSD");
        let derived = schema
            .to_arrow_schema("big", ArrowOptions::default())
            .expect("a finite schema of this size must derive");
        assert_eq!(derived.fields().len(), 6);
    }

    #[test]
    fn an_explosive_schema_is_still_refused() {
        // Twelve children of the element's own type: flattening to the
        // default depth of 8 would emit 12^8 columns.
        let schema = load(&self_recursive_type(12)).expect("a recursive type is legal XSD");
        let message =
            resource_limit_message(schema.to_arrow_schema("big", ArrowOptions::default()));
        assert!(message.contains("arrow_derivation_steps"), "{message}");

        // The same without recursion: 20^6 leaf columns from 4 KiB of schema.
        let schema = load(&named_type_tree(20, 6)).expect("a tree of named types is legal XSD");
        let message =
            resource_limit_message(schema.to_arrow_schema("big", ArrowOptions::default()));
        assert!(message.contains("arrow_derivation_steps"), "{message}");
    }
}
