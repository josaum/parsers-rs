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

mod includes {
    use super::{resource_limit_message, schema_document};
    use xsd_fast_core::{ParseOptions, SchemaSet};

    /// Loads a root schema followed by `includes` files, each of which is
    /// included by the one before it.
    fn load_chain(includes: usize) -> anyhow::Result<SchemaSet> {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..=includes {
            let mut body = format!("<xs:element name=\"e{index}\" type=\"xs:string\"/>");
            if index < includes {
                body.push_str(&format!(
                    "<xs:include schemaLocation=\"f{}.xsd\"/>",
                    index + 1
                ));
            }
            std::fs::write(
                dir.path().join(format!("f{index}.xsd")),
                schema_document(&body),
            )
            .expect("write schema file");
        }
        // Leave the file count out of it: only the chain depth is under test.
        let options = ParseOptions {
            max_includes: 1_000,
            ..ParseOptions::default()
        };
        SchemaSet::load(dir.path().join("f0.xsd"), options)
    }

    #[test]
    fn the_include_chain_limit_is_the_number_in_its_message() {
        let schema = load_chain(32).expect("32 files below the root are within the limit");
        assert_eq!(schema.model().global_elements.len(), 33);

        let message = resource_limit_message(load_chain(33));
        assert!(message.contains("schema_include_nesting"), "{message}");
        assert!(
            message.contains("deeper than 32 files below the root schema"),
            "{message}"
        );
    }
}

mod inheritance {
    use super::{load, resource_limit_message, schema_document};
    use xsd_fast_core::schema::{ContentModel, ElementRef, XsdType};

    /// A base type whose content is `base_content`, and `derived` types that
    /// extend it without adding anything.
    fn base_with_derived_types(base_content: &str, derived: usize) -> Vec<u8> {
        let mut body = format!(
            "<xs:complexType name=\"Base\"><xs:sequence>{base_content}</xs:sequence>\
             </xs:complexType>"
        );
        for index in 0..derived {
            body.push_str(&format!(
                "<xs:complexType name=\"D{index}\"><xs:complexContent>\
                 <xs:extension base=\"t:Base\"><xs:sequence/></xs:extension>\
                 </xs:complexContent></xs:complexType>"
            ));
        }
        schema_document(&body)
    }

    /// An element whose inline simple type lists `values` enumeration values.
    fn enumerated_element(values: usize) -> String {
        let mut element = String::from(
            "<xs:element name=\"code\"><xs:simpleType><xs:restriction base=\"xs:string\">",
        );
        for value in 0..values {
            element.push_str(&format!("<xs:enumeration value=\"{value}\"/>"));
        }
        element.push_str("</xs:restriction></xs:simpleType></xs:element>");
        element
    }

    #[test]
    fn copies_of_a_large_enumeration_are_charged() {
        // 2,000 values copied into 200 derived types is 400,000 enumeration
        // values, although only 200 particles are copied.
        let document = base_with_derived_types(&enumerated_element(2_000), 200);
        assert!(document.len() < 128 * 1024);
        let message = resource_limit_message(load(&document));
        assert!(message.contains("inherited_nodes"), "{message}");
    }

    #[test]
    fn copies_of_long_text_are_charged() {
        // One 64 KiB enumeration value copied into 1,100 derived types is
        // 70 MiB of text in 1,100 particles.
        let value = "x".repeat(64 * 1024);
        let element = format!(
            "<xs:element name=\"code\"><xs:simpleType><xs:restriction base=\"xs:string\">\
             <xs:enumeration value=\"{value}\"/></xs:restriction></xs:simpleType></xs:element>"
        );
        let message = resource_limit_message(load(&base_with_derived_types(&element, 1_100)));
        assert!(message.contains("inherited_nodes"), "{message}");

        // The same for one element with a 64 KiB name.
        let element = format!("<xs:element name=\"{value}\" type=\"xs:string\"/>");
        let message = resource_limit_message(load(&base_with_derived_types(&element, 1_100)));
        assert!(message.contains("inherited_nodes"), "{message}");
    }

    #[test]
    fn ordinary_inheritance_still_folds() {
        // An enumeration of 20 values inherited by 20 types.
        let document = base_with_derived_types(&enumerated_element(20), 20);
        let schema = load(&document).expect("a small enumeration is inherited");
        let ContentModel::Sequence(particles) = &schema.model().complex_types["D19"].content else {
            panic!("the derived type has a sequence");
        };
        let ElementRef::Inline(code) = &particles[0].element else {
            panic!("the inherited element is declared inline");
        };
        let XsdType::Simple(simple) = &code.type_def else {
            panic!("the inherited element has a simple type");
        };
        assert_eq!(simple.facets.enumerations.len(), 20);

        // 50 plain elements inherited by 1,000 types: 50,000 copies.
        let mut elements = String::new();
        for index in 0..50 {
            elements.push_str(&format!("<xs:element name=\"e{index}\" type=\"xs:int\"/>"));
        }
        let schema = load(&base_with_derived_types(&elements, 1_000))
            .expect("50,000 inherited particles are within the limit");
        let ContentModel::Sequence(particles) = &schema.model().complex_types["D999"].content
        else {
            panic!("the derived type has a sequence");
        };
        assert_eq!(particles.len(), 50);
    }
}

mod validation {
    use super::{load, schema_document};
    use xsd_fast_core::{SchemaSet, ValidateOptions, ValidationError};

    /// Three `e` values outside the enumeration and no required `req` child.
    const INVALID: &[u8] = b"<r><e>B</e><e>C</e><e>D</e></r>";
    /// The end tag of `req` does not match.
    const NOT_WELL_FORMED: &[u8] = b"<r><req>x</wrong></r>";

    fn schema() -> SchemaSet {
        load(&schema_document(
            "<xs:simpleType name=\"E\"><xs:restriction base=\"xs:string\">\
             <xs:enumeration value=\"A\"/></xs:restriction></xs:simpleType>\
             <xs:element name=\"r\"><xs:complexType><xs:sequence>\
             <xs:element name=\"e\" type=\"t:E\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>\
             <xs:element name=\"req\" type=\"xs:string\"/>\
             </xs:sequence></xs:complexType></xs:element>",
        ))
        .expect("schema loads")
    }

    fn validate(
        schema: &SchemaSet,
        xml: &[u8],
        max_errors: usize,
    ) -> anyhow::Result<Vec<ValidationError>> {
        schema.validate(
            xml,
            ValidateOptions {
                max_errors,
                ..ValidateOptions::default()
            },
        )
    }

    fn parse_errors(findings: &[ValidationError]) -> usize {
        findings
            .iter()
            .filter(|finding| finding.message.starts_with("XML parse error"))
            .count()
    }

    #[test]
    fn a_limit_of_zero_is_refused_not_reported_as_clean() {
        let schema = schema();
        for xml in [INVALID, NOT_WELL_FORMED, b"<r><req>x</req></r>"] {
            let error = validate(&schema, xml, 0).expect_err("a limit of zero must be refused");
            let message = error.to_string();
            assert!(message.contains("max_errors"), "{message}");
            assert!(message.contains("positive limit"), "{message}");
        }
    }

    #[test]
    fn findings_stop_exactly_at_the_limit() {
        let schema = schema();
        // The document has four findings: three values and the missing child.
        for (max_errors, expected) in [(1, 1), (3, 3), (4, 4), (5, 4), (100, 4)] {
            let findings = validate(&schema, INVALID, max_errors).expect("validation runs");
            assert_eq!(findings.len(), expected, "max_errors = {max_errors}");
            assert_eq!(parse_errors(&findings), 0);
        }
    }

    #[test]
    fn a_parse_error_is_reported_under_every_limit() {
        let schema = schema();
        for max_errors in [1, 100] {
            let findings = validate(&schema, NOT_WELL_FORMED, max_errors).expect("validation runs");
            assert_eq!(findings.len(), 1, "max_errors = {max_errors}");
            assert_eq!(parse_errors(&findings), 1);
        }
    }

    #[test]
    fn a_parse_error_past_the_limit_is_still_reported() {
        let schema = schema();
        // Two invalid values reach a limit of two; the mismatched end tag
        // comes after them.
        let xml = b"<r><e>B</e><e>C</e><req>x</wrong></r>";
        let findings = validate(&schema, xml, 2).expect("validation runs");
        assert_eq!(findings.len(), 3, "{findings:?}");
        assert_eq!(parse_errors(&findings), 1);
        assert_eq!(findings[2].path, "/r/req");

        // The same document closed properly stays at the limit.
        let xml = b"<r><e>B</e><e>C</e><req>x</req></r>";
        let findings = validate(&schema, xml, 2).expect("validation runs");
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(parse_errors(&findings), 0);
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

    #[test]
    fn all_arrow_schemas_omits_what_try_all_arrow_schemas_reports() {
        // `big` cannot be derived and `small` can.
        let schema = load(&self_recursive_type(12)).expect("a recursive type is legal XSD");
        assert_eq!(schema.model().global_elements.len(), 2);

        let derived = schema.all_arrow_schemas(ArrowOptions::default());
        let names: Vec<&str> = derived.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["small"], "the element over the limit is omitted");

        // Callers match on the plain message, so the limit must lead it.
        let error = schema
            .try_all_arrow_schemas(ArrowOptions::default())
            .expect_err("an element over the limit must fail the strict form");
        let message = error.to_string();
        assert!(
            message.starts_with("resource_limit: arrow_derivation_steps"),
            "{message}"
        );
        assert!(message.ends_with("(element 'big')"), "{message}");
    }

    #[test]
    fn try_all_arrow_schemas_returns_every_element() {
        let schema = load(&named_type_tree(3, 3)).expect("a tree of named types is legal XSD");
        let lenient = schema.all_arrow_schemas(ArrowOptions::default());
        let strict = schema
            .try_all_arrow_schemas(ArrowOptions::default())
            .expect("every element of a small schema derives");
        let names: Vec<&str> = strict.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["big", "small"]);
        assert_eq!(strict, lenient);
    }
}
