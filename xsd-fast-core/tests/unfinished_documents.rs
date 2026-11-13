//! Documents that cannot be valid whatever their values are.
//!
//! A document that ends with elements still open is not well-formed, and a
//! document whose root element the schema does not declare was not checked
//! against anything. Neither may be reported as clean.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, ValidationError};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="e" maxOccurs="unbounded">
          <xs:simpleType>
            <xs:restriction base="xs:string"><xs:enumeration value="A"/></xs:restriction>
          </xs:simpleType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

fn findings(document: &str, max_errors: usize) -> Vec<ValidationError> {
    let options = ValidateOptions {
        max_errors,
        ..ValidateOptions::default()
    };
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), options)
        .expect("validation runs")
}

#[test]
fn a_document_that_ends_with_elements_open_is_reported() {
    for document in ["<r><e>B", "<r><e>A</e>", "<r>", "<r><e>A</e><e>"] {
        let found = findings(document, 100);
        assert_eq!(found.len(), 1, "{document}: {found:?}");
        assert!(
            found[0].message.contains("still open"),
            "{document}: {found:?}"
        );
        assert!(found[0].path.starts_with("/r"), "{document}: {found:?}");
    }
}

#[test]
fn an_unfinished_document_is_reported_beyond_the_finding_limit() {
    // Two values are refused, which reaches the limit; the document then ends
    // inside <r>.
    let found = findings("<r><e>B</e><e>C</e><e>A</e>", 2);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found[2].message.contains("still open"), "{found:?}");
}

#[test]
fn a_root_element_the_schema_does_not_declare_is_reported() {
    for document in [
        "<other><e>B</e></other>",
        "<other/>",
        "<x:other xmlns:x=\"urn:x\"/>",
    ] {
        let found = findings(document, 100);
        assert_eq!(found.len(), 1, "{document}: {found:?}");
        assert_eq!(found[0].path, "/other", "{document}");
        assert!(
            found[0].message.contains("not declared"),
            "{document}: {found:?}"
        );
    }
}

#[test]
fn complete_documents_with_a_declared_root_are_unaffected() {
    assert!(findings("<r><e>A</e></r>", 100).is_empty());
    assert!(findings(
        "<?xml version=\"1.0\"?>\n<r><e>A</e></r>\n<!-- end -->\n",
        100
    )
    .is_empty());
}
