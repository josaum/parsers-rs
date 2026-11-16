//! `xsi:nil` is recognised by namespace, not by prefix.
//!
//! The attribute is `nil` in the XML Schema instance namespace, whatever
//! prefix the document binds to it. A `nil` attribute in any other namespace
//! is the document's own and does not empty the element.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="n" nillable="true"><xs:complexType><xs:sequence>
    <xs:element name="req" type="xs:string"/>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="outer"><xs:complexType><xs:sequence>
    <xs:element name="inner" nillable="true"><xs:complexType><xs:sequence>
      <xs:element name="req" type="xs:string"/>
    </xs:sequence></xs:complexType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const MISSING: &str = "required child <req> is missing";

fn messages(document: &str) -> Vec<String> {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

#[test]
fn a_nil_attribute_in_another_namespace_is_not_xsi_nil() {
    for document in [
        r#"<n xmlns:other="urn:other" other:nil="true"/>"#.to_string(),
        r#"<n xmlns:other="urn:other" other:nil="true"></n>"#.to_string(),
        // The prefix is not bound at all.
        r#"<n xsi:nil="true"/>"#.to_string(),
    ] {
        assert_eq!(messages(&document), [MISSING], "{document}");
    }
}

#[test]
fn the_instance_namespace_counts_under_any_prefix() {
    for document in [
        format!(r#"<n xmlns:xsi="{XSI}" xsi:nil="true"/>"#),
        format!(r#"<n xmlns:i="{XSI}" i:nil="true"></n>"#),
        // Declared on an ancestor.
        format!(r#"<outer xmlns:i="{XSI}"><inner i:nil="true"/></outer>"#),
    ] {
        assert_eq!(messages(&document), Vec::<String>::new(), "{document}");
    }
}

#[test]
fn a_prefix_rebound_by_a_descendant_no_longer_means_nil() {
    let document = format!(
        r#"<outer xmlns:xsi="{XSI}"><inner xmlns:xsi="urn:other" xsi:nil="true"/></outer>"#
    );
    assert_eq!(messages(&document), [MISSING]);
    // The rebinding ends with the element that made it.
    let document =
        format!(r#"<outer xmlns:xsi="{XSI}"><inner xsi:nil="true"><!-- nil --></inner></outer>"#);
    assert_eq!(messages(&document), Vec::<String>::new());
}
