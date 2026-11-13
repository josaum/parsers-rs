//! A document has exactly one root element and nothing but it.
//!
//! Input with no element, character data outside the root and a second
//! top-level element are not well-formed documents, so none is reported
//! clean, even when every element in it is declared and valid.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r"><xs:complexType><xs:sequence>
    <xs:element name="e" minOccurs="0"><xs:simpleType>
      <xs:restriction base="xs:string"><xs:enumeration value="A"/></xs:restriction>
    </xs:simpleType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;

fn findings(document: &str, max_errors: usize) -> Vec<(String, String)> {
    let options = ValidateOptions {
        max_errors,
        ..ValidateOptions::default()
    };
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), options)
        .expect("validation runs")
        .into_iter()
        .map(|finding| (finding.path, finding.message))
        .collect()
}

#[test]
fn input_without_an_element_has_no_root() {
    for document in [
        "",
        "  \n\t",
        "<!-- only a comment -->",
        "<?xml version=\"1.0\"?>\n",
    ] {
        let found = findings(document, 100);
        assert_eq!(found.len(), 1, "{document:?}: {found:?}");
        assert_eq!(found[0].0, "/");
        assert_eq!(
            found[0].1, "XML parse error: no root element",
            "{document:?}"
        );
    }
}

#[test]
fn character_data_outside_the_root_is_reported() {
    for document in [
        "<r/>trailing",
        "<r><e>A</e></r>\n x",
        "<r/><![CDATA[x]]>",
        "<r/>&amp;",
        "junk<r/>",
    ] {
        let found = findings(document, 100);
        assert_eq!(found.len(), 1, "{document:?}: {found:?}");
        assert!(
            found[0]
                .1
                .contains("character data outside the root element"),
            "{found:?}"
        );
    }
}

#[test]
fn a_second_top_level_element_is_reported_even_when_declared() {
    for document in [
        "<r/><r/>",
        "<r><e>A</e></r>\n<r><e>A</e></r>",
        "<r/><r/><r/>",
    ] {
        let found = findings(document, 100);
        assert_eq!(found.len(), 1, "{document:?}: {found:?}");
        assert_eq!(found[0].0, "/r");
        assert!(
            found[0].1.contains("second top-level element <r>"),
            "{found:?}"
        );
    }
    // The second element is still validated.
    let found = findings("<r/><r><e>B</e></r>", 100);
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn these_findings_are_reported_beyond_the_limit() {
    let found = findings("<r><e>B</e></r><r/>", 1);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[1].1.contains("second top-level element"), "{found:?}");
    let found = findings("<r><e>B</e></r>tail", 1);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[1].1.contains("character data outside"), "{found:?}");
}

#[test]
fn one_root_with_only_comments_and_whitespace_around_it_is_clean() {
    let document = "<?xml version=\"1.0\"?>\n<!-- a -->\n<r><e>A</e></r>\n<!-- b -->\n<?pi x?>\n";
    assert_eq!(findings(document, 100), []);
}
