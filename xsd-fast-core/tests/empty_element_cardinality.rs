//! A self-closing element is an element with no children.
//!
//! `<c/>` and `<c></c>` are the same document, so both must report the
//! required children that are missing.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="c" maxOccurs="unbounded">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="must" type="xs:string"/>
              <xs:element name="may" type="xs:string" minOccurs="0"/>
            </xs:sequence>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

fn findings(document: &str, options: ValidateOptions) -> Vec<(String, String)> {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), options)
        .expect("validation runs")
        .into_iter()
        .map(|finding| (finding.path, finding.message))
        .collect()
}

#[test]
fn a_self_closing_element_reports_its_missing_children() {
    let open_close = findings("<r><c></c></r>", ValidateOptions::default());
    assert_eq!(
        open_close,
        [(
            "/r/c".to_string(),
            "required child <must> is missing".to_string()
        )]
    );
    assert_eq!(
        findings("<r><c/></r>", ValidateOptions::default()),
        open_close
    );
}

#[test]
fn a_self_closing_root_reports_its_missing_children() {
    assert_eq!(
        findings("<r/>", ValidateOptions::default()),
        findings("<r></r>", ValidateOptions::default())
    );
    assert_eq!(findings("<r/>", ValidateOptions::default()).len(), 1);
}

#[test]
fn a_self_closing_element_is_not_checked_when_cardinality_is_off() {
    let options = ValidateOptions {
        check_cardinality: false,
        ..ValidateOptions::default()
    };
    assert_eq!(findings("<r><c/></r>", options), []);
}
