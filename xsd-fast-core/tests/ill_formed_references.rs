//! References that make a document ill-formed, and entities that do not.
//!
//! A bare `&`, a character reference to no character and an entity that is
//! neither predefined nor declared in the document's DOCTYPE are not
//! well-formed XML. The value is still validated as written, but the
//! document is never reported clean. An entity declared in the internal
//! DOCTYPE subset with literal replacement text is well-formed and resolves.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r"><xs:complexType><xs:sequence>
    <xs:element name="e" maxOccurs="unbounded"><xs:simpleType>
      <xs:restriction base="xs:string"><xs:enumeration value="A"/><xs:enumeration value="a &amp; b"/></xs:restriction>
    </xs:simpleType></xs:element>
  </xs:sequence><xs:attribute name="a" type="xs:string"/></xs:complexType></xs:element>
</xs:schema>"#;

fn messages(document: &str, max_errors: usize) -> Vec<String> {
    let options = ValidateOptions {
        max_errors,
        ..ValidateOptions::default()
    };
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), options)
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

fn is_parse_error(message: &str) -> bool {
    message.starts_with("XML parse error: ")
}

#[test]
fn ill_formed_element_text_is_reported_and_still_validated() {
    for (written, kept) in [
        ("a & b", None),
        ("&foo;", Some("&foo;")),
        ("&#0;", Some("&#0;")),
        ("&amp", Some("&amp")),
        ("A&#xD800;", Some("A&#xD800;")),
    ] {
        let found = messages(&format!("<r><e>{written}</e></r>"), 100);
        let parse_errors = found.iter().filter(|m| is_parse_error(m)).count();
        assert_eq!(parse_errors, 1, "{written}: {found:?}");
        // The value is checked as written: `a & b` is an allowed value, the
        // others are refused for what they hold.
        match kept {
            None => assert_eq!(found.len(), 1, "{written}: {found:?}"),
            Some(kept) => {
                assert_eq!(found.len(), 2, "{written}: {found:?}");
                assert!(
                    found
                        .iter()
                        .any(|m| m.contains(&format!("value {kept:?} not in enumeration"))),
                    "{written}: {found:?}"
                );
            }
        }
    }
}

#[test]
fn ill_formed_attribute_values_are_reported() {
    for written in ["x & y", "&foo;", "&#0;", "&amp"] {
        let found = messages(&format!("<r a=\"{written}\"><e>A</e></r>"), 100);
        assert_eq!(found.len(), 1, "{written}: {found:?}");
        assert!(is_parse_error(&found[0]), "{written}: {found:?}");
    }
}

#[test]
fn one_finding_is_reported_per_document_even_at_the_limit() {
    let found = messages("<r><e>B</e><e>&foo;</e><e>a & b</e><e>&bar;</e></r>", 1);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(is_parse_error(&found[1]), "{found:?}");
    let found = messages("<r><e>&foo;</e><e>&bar;</e></r>", 100);
    assert_eq!(
        found.iter().filter(|m| is_parse_error(m)).count(),
        1,
        "{found:?}"
    );
}

#[test]
fn well_formed_references_are_not_reported() {
    for written in ["a &amp; b", "a &#38; b", "<![CDATA[a & b]]>", "A"] {
        assert_eq!(
            messages(&format!("<r a=\"&lt;&#233;\"><e>{written}</e></r>"), 100),
            Vec::<String>::new(),
            "{written}"
        );
    }
}

#[test]
fn an_entity_declared_in_the_internal_subset_resolves() {
    let doctype = "<!DOCTYPE r [\n<!ENTITY val \"A\">\n<!ENTITY both 'a &amp; b'>\n<!ENTITY % param \"ignored\">\n<!ENTITY ext SYSTEM \"file:///etc/passwd\">\n<!ENTITY nested \"&val;\">\n]>";
    assert_eq!(
        messages(
            &format!("{doctype}<r a=\"&val;\"><e>&val;</e><e> &val; </e></r>"),
            100
        ),
        Vec::<String>::new()
    );
    // Replacement text holding a reference, an external entity and an
    // undeclared name are not resolved: the reference is kept and reported.
    for name in ["both", "ext", "nested", "param", "missing"] {
        let found = messages(&format!("{doctype}<r><e>&{name};</e></r>"), 100);
        assert_eq!(found.len(), 2, "{name}: {found:?}");
        assert!(found.iter().any(|m| is_parse_error(m)), "{name}: {found:?}");
        assert!(
            found.iter().any(|m| m.contains(&format!("\"&{name};\""))),
            "{name}: {found:?}"
        );
    }
    // A predefined entity cannot be redefined.
    let found = messages("<!DOCTYPE r [<!ENTITY amp \"A\">]><r><e>&amp;</e></r>", 100);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("\"&\""), "{found:?}");
}

#[test]
fn declared_entities_are_bounded() {
    let mut doctype = String::from("<!DOCTYPE r [");
    for index in 0..300 {
        doctype.push_str(&format!("<!ENTITY e{index} \"A\">"));
    }
    doctype.push_str(&format!("<!ENTITY big \"{}\">]>", "A".repeat(5000)));
    assert_eq!(
        messages(&format!("{doctype}<r><e>&e0;</e><e>&e255;</e></r>"), 100),
        Vec::<String>::new()
    );
    for name in ["e256", "e299", "big"] {
        let found = messages(&format!("{doctype}<r><e>&{name};</e></r>"), 100);
        assert!(found.iter().any(|m| is_parse_error(m)), "{name}: {found:?}");
    }
}
