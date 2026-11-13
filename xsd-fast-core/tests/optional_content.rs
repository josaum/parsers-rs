//! Content that a valid document may leave out.
//!
//! An optional model group, an optional choice and a nilled element are all
//! valid when empty, whether the element is written `<a/>` or `<a></a>`.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="a"><xs:complexType>
    <xs:choice minOccurs="0"><xs:element name="x" type="xs:string"/><xs:element name="y" type="xs:string"/></xs:choice>
  </xs:complexType></xs:element>
  <xs:element name="b"><xs:complexType><xs:sequence>
    <xs:element name="k" type="xs:string"/>
    <xs:choice minOccurs="0"><xs:element name="x" type="xs:string"/><xs:element name="y" type="xs:string"/></xs:choice>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="c"><xs:complexType>
    <xs:sequence minOccurs="0"><xs:element name="req" type="xs:string"/><xs:element name="opt" type="xs:string" minOccurs="0"/></xs:sequence>
  </xs:complexType></xs:element>
  <xs:element name="d"><xs:complexType>
    <xs:all minOccurs="0"><xs:element name="req" type="xs:string"/><xs:element name="opt" type="xs:string" minOccurs="0"/></xs:all>
  </xs:complexType></xs:element>
  <xs:element name="n" nillable="true"><xs:complexType><xs:sequence>
    <xs:element name="req" type="xs:string"/>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="strict"><xs:complexType><xs:sequence>
    <xs:element name="req" type="xs:string"/>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="e"><xs:complexType>
    <xs:choice><xs:element name="x" type="xs:string"/><xs:element name="y" type="xs:string"/></xs:choice>
  </xs:complexType></xs:element>
</xs:schema>"#;

const XSI: &str = "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"";

fn messages(document: &str) -> Vec<String> {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema parses")
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

/// Both spellings of an element named `name` with `attributes` and no content.
fn both(name: &str, attributes: &str) -> [String; 2] {
    [
        format!("<{name}{attributes}/>"),
        format!("<{name}{attributes}></{name}>"),
    ]
}

#[test]
fn optional_content_may_be_left_out_in_both_spellings() {
    for name in ["a", "c", "d"] {
        for document in both(name, "") {
            assert_eq!(messages(&document), Vec::<String>::new(), "{document}");
        }
    }
    assert_eq!(messages("<b><k>1</k></b>"), Vec::<String>::new());
    for document in both("n", &format!(" {XSI} xsi:nil=\"true\"")) {
        assert_eq!(messages(&document), Vec::<String>::new(), "{document}");
    }
}

#[test]
fn what_is_required_is_still_required() {
    // A required choice with nothing chosen.
    for document in both("e", "") {
        assert_eq!(messages(&document).len(), 1, "{document}");
    }
    // An optional group that is present must be complete.
    for name in ["c", "d"] {
        let found = messages(&format!("<{name}><opt>1</opt></{name}>"));
        assert_eq!(found, ["required child <req> is missing"], "{name}");
    }
    // xsi:nil counts only on a nillable element, and only when true.
    for document in both("strict", &format!(" {XSI} xsi:nil=\"true\"")) {
        assert_eq!(
            messages(&document),
            ["required child <req> is missing"],
            "{document}"
        );
    }
    for document in both("n", &format!(" {XSI} xsi:nil=\"false\"")) {
        assert_eq!(
            messages(&document),
            ["required child <req> is missing"],
            "{document}"
        );
    }
    for document in both("n", "") {
        assert_eq!(
            messages(&document),
            ["required child <req> is missing"],
            "{document}"
        );
    }
}

#[test]
fn a_model_serialized_without_the_group_occurrence_still_loads() {
    let json = r#"{"name":null,"content":"Empty","attributes":[]}"#;
    let complex: xsd_fast_core::ComplexType = serde_json::from_str(json).expect("loads");
    assert_eq!(complex.content_occurrence.min, 1);
    assert_eq!(complex.content_occurrence.max, Some(1));
}
