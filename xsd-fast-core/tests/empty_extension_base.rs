//! `<xs:extension base="..."/>` written as a self-closing tag.
//!
//! An extension that adds nothing is still a derivation: the type has
//! everything its base has.

use xsd_fast_core::{ContentModel, ParseOptions, SchemaSet, SimpleTypeKind, ValidateOptions};

fn schema(extension: &str, simple_extension: &str) -> Vec<u8> {
    format!(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:complexType name="Base">
    <xs:sequence>
      <xs:element name="id" type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="code" type="xs:int"/>
  </xs:complexType>
  <xs:complexType name="Same">
    <xs:complexContent>{extension}</xs:complexContent>
  </xs:complexType>
  <xs:complexType name="Text">
    <xs:simpleContent>{simple_extension}</xs:simpleContent>
  </xs:complexType>
  <xs:element name="same" type="t:Same"/>
</xs:schema>"#
    )
    .into_bytes()
}

#[test]
fn a_self_closing_extension_keeps_its_base() {
    let self_closing = SchemaSet::load_bytes(
        &schema(
            r#"<xs:extension base="t:Base"/>"#,
            r#"<xs:extension base="xs:int"/>"#,
        ),
        ParseOptions::default(),
    )
    .expect("schema parses");
    let open_close = SchemaSet::load_bytes(
        &schema(
            r#"<xs:extension base="t:Base"></xs:extension>"#,
            r#"<xs:extension base="xs:int"></xs:extension>"#,
        ),
        ParseOptions::default(),
    )
    .expect("schema parses");

    let same = &self_closing.model().complex_types["Same"];
    assert_eq!(
        same.base.as_ref().map(|base| base.local.as_str()),
        Some("Base")
    );
    assert!(matches!(&same.content, ContentModel::Sequence(p) if p.len() == 1));
    assert_eq!(same.attributes.len(), 1);
    let text = &self_closing.model().complex_types["Text"];
    assert!(
        matches!(&text.content, ContentModel::SimpleContent(simple) if simple.kind == SimpleTypeKind::Int)
    );
    assert_eq!(
        serde_json::to_string(self_closing.model()).expect("serializes"),
        serde_json::to_string(open_close.model()).expect("serializes")
    );

    // The inherited required child is enforced.
    let findings = self_closing
        .validate(b"<same></same>", ValidateOptions::default())
        .expect("validation runs");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].message.contains("<id>"), "{findings:?}");
}
