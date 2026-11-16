//! An `xs:any` branch of a choice is satisfied by a child that no named
//! particle of the type claims, in a namespace the wildcard admits.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

const SCHEMA: &[u8] = br###"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t" elementFormDefault="qualified">
  <xs:element name="other"><xs:complexType><xs:choice>
    <xs:element name="w1" type="xs:int"/><xs:any namespace="##other" processContents="lax"/>
  </xs:choice></xs:complexType></xs:element>
  <xs:element name="anything"><xs:complexType><xs:sequence>
    <xs:element name="head" type="xs:string"/>
    <xs:choice><xs:element name="w1" type="xs:int"/><xs:any/></xs:choice>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="listed"><xs:complexType><xs:choice>
    <xs:element name="w1" type="xs:int"/><xs:any namespace="urn:a ##local"/>
  </xs:choice></xs:complexType></xs:element>
</xs:schema>"###;

fn messages(xml: &str) -> Vec<String> {
    SchemaSet::load_bytes(SCHEMA, ParseOptions::default())
        .expect("schema loads")
        .validate(xml.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| format!("{} {}", finding.path, finding.message))
        .collect()
}

const NONE_PRESENT: &str = "choice requires exactly one branch, none present";

#[test]
fn a_child_in_another_namespace_selects_an_other_wildcard() {
    assert!(messages(r##"<other xmlns="urn:t"><x:foo xmlns:x="urn:o"/></other>"##).is_empty());
}

#[test]
fn a_child_in_the_target_namespace_does_not_select_an_other_wildcard() {
    // Reported on main too.
    assert_eq!(
        messages(r##"<other xmlns="urn:t"><foo/></other>"##),
        [format!("/other {NONE_PRESENT}")]
    );
}

#[test]
fn a_named_branch_claims_its_child_before_the_wildcard() {
    // Passes on main too: the child is the named branch, checked by its type.
    assert_eq!(
        messages(r##"<other xmlns="urn:t"><w1>x</w1></other>"##),
        ["/other/w1 expected numeric value for Int, got \"x\""]
    );
}

#[test]
fn an_any_wildcard_in_a_nested_choice_takes_any_unclaimed_child() {
    assert!(messages(r##"<anything xmlns="urn:t"><head/><zzz/></anything>"##).is_empty());
    assert!(
        messages(r##"<anything xmlns="urn:t"><head/><x:zzz xmlns:x="urn:o"/></anything>"##)
            .is_empty()
    );
}

#[test]
fn a_namespace_list_admits_what_it_lists() {
    assert!(
        messages(r##"<t:listed xmlns:t="urn:t"><a:x xmlns:a="urn:a"/></t:listed>"##).is_empty()
    );
    assert!(messages(r##"<t:listed xmlns:t="urn:t"><x/></t:listed>"##).is_empty());
    assert_eq!(
        messages(r##"<t:listed xmlns:t="urn:t"><t:x/></t:listed>"##),
        [format!("/listed {NONE_PRESENT}")]
    );
}

#[cfg(feature = "arrow")]
#[test]
fn a_wildcard_is_no_column() {
    use xsd_fast_core::arrow::ArrowOptions;
    let schema = SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("schema loads");
    let arrow = schema
        .to_arrow_schema("other", ArrowOptions::default())
        .expect("other derives");
    let names: Vec<_> = arrow.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["w1"]);
}

const OPTIONAL: &[u8] = br###"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:t="urn:t" targetNamespace="urn:t">
  <xs:element name="a"><xs:complexType><xs:choice><xs:any minOccurs="0"/></xs:choice></xs:complexType></xs:element>
  <xs:element name="b"><xs:complexType><xs:sequence>
    <xs:sequence minOccurs="0"><xs:element name="req" type="xs:string"/><xs:any minOccurs="0"/></xs:sequence>
    <xs:any minOccurs="0"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"###;

fn optional_messages(xml: &str) -> Vec<String> {
    SchemaSet::load_bytes(OPTIONAL, ParseOptions::default())
        .expect("schema loads")
        .validate(xml.as_bytes(), ValidateOptions::default())
        .expect("validation runs")
        .into_iter()
        .map(|finding| format!("{} {}", finding.path, finding.message))
        .collect()
}

#[test]
fn a_choice_whose_wildcard_may_be_left_out_may_be_empty() {
    assert!(optional_messages(r##"<a xmlns="urn:t"/>"##).is_empty());
}

#[test]
fn an_unclaimed_child_does_not_select_an_optional_sequence() {
    // The child may be the outer wildcard's; nothing requires `req`.
    assert!(optional_messages(r##"<b xmlns="urn:t"><x/></b>"##).is_empty());
}
