//! Element text reaches facet validation as the characters it stands for.
//!
//! Entity references, character references and CDATA sections are three ways
//! to write the same text. A validator that drops or mangles any of them
//! reports valid documents as invalid, or checks a length against the wrong
//! string.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, ValidationSeverity};

const SCHEMA: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" elementFormDefault="qualified">
  <xs:simpleType name="choice">
    <xs:restriction base="xs:string">
      <xs:enumeration value="a &amp; b"/>
      <xs:enumeration value="caf&#233;"/>
      <xs:enumeration value="x&lt;y"/>
      <xs:enumeration value="&#x1F600;"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="three">
    <xs:restriction base="xs:string">
      <xs:maxLength value="3"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="v" type="choice" minOccurs="0" maxOccurs="unbounded"/>
        <xs:element name="n" type="three" minOccurs="0" maxOccurs="unbounded"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>
"#;

/// Messages of the error-severity findings for `<r>` holding `body`.
fn errors_for(body: &str) -> Vec<String> {
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema parses");
    let document = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<r>{body}</r>\n");
    schema
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .filter(|finding| finding.severity == ValidationSeverity::Error)
        .map(|finding| finding.message)
        .collect()
}

#[test]
fn predefined_entities_are_the_characters_they_name() {
    assert_eq!(errors_for("<v>a &amp; b</v>"), Vec::<String>::new());
    assert_eq!(errors_for("<v>x&lt;y</v>"), Vec::<String>::new());
}

#[test]
fn character_references_are_the_characters_they_number() {
    assert_eq!(errors_for("<v>caf&#233;</v>"), Vec::<String>::new());
    assert_eq!(errors_for("<v>caf&#xE9;</v>"), Vec::<String>::new());
    assert_eq!(errors_for("<v>&#128512;</v>"), Vec::<String>::new());
    assert_eq!(errors_for("<v>a &#38; b</v>"), Vec::<String>::new());
}

#[test]
fn a_value_outside_the_enumeration_is_still_refused() {
    let errors = errors_for("<v>a &amp; c</v>");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("enumeration"), "{errors:?}");
}

#[test]
fn length_is_counted_in_characters_after_references_are_resolved() {
    // Three characters, written as fifteen bytes.
    assert_eq!(errors_for("<n>&amp;&#38;&#x26;</n>"), Vec::<String>::new());
    // Four characters.
    let errors = errors_for("<n>&amp;&amp;&amp;&amp;</n>");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("maxLength"), "{errors:?}");
}

#[test]
fn cdata_is_element_text() {
    assert_eq!(errors_for("<v><![CDATA[a & b]]></v>"), Vec::<String>::new());
    assert_eq!(errors_for("<v>a <![CDATA[&]]> b</v>"), Vec::<String>::new());
    // Whitespace around the value is still not part of it.
    assert_eq!(
        errors_for("<v>\n   <![CDATA[a & b]]>\n  </v>"),
        Vec::<String>::new()
    );
    assert_eq!(errors_for("<v>  caf&#233; </v>"), Vec::<String>::new());
    let errors = errors_for("<n><![CDATA[<<<<]]></n>");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("maxLength"), "{errors:?}");
}
