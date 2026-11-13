//! A sequence that is one branch of a choice stays a group in the model,
//! and validation reads its particles as the choice's own, as it did when
//! the parser wrote them into the choice.

use xsd_fast_core::schema::{ContentModel, ElementRef};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, ValidationSeverity};

const XSD: &str = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r"><xs:complexType><xs:choice>
    <xs:sequence>
      <xs:element name="a" type="xs:string"/>
      <xs:element name="b" type="xs:string"/>
    </xs:sequence>
    <xs:sequence>
      <xs:element name="a" type="xs:string"/>
      <xs:element name="c" type="xs:string"/>
    </xs:sequence>
  </xs:choice></xs:complexType></xs:element>
</xs:schema>"#;

fn load() -> SchemaSet {
    SchemaSet::load_bytes(XSD.as_bytes(), ParseOptions::default()).expect("schema loads")
}

fn errors(xml: &str) -> Vec<String> {
    load()
        .validate(xml.as_bytes(), ValidateOptions::default())
        .expect("document reads")
        .into_iter()
        .filter(|e| e.severity == ValidationSeverity::Error)
        .map(|e| e.message)
        .collect()
}

#[test]
fn each_sequence_branch_is_one_group_particle() {
    let set = load();
    let decl = set.model().global_elements.get("r").expect("r is declared");
    let xsd_fast_core::XsdType::Complex(ct) = &decl.type_def else {
        panic!("r has an inline complex type");
    };
    let ContentModel::Choice(branches) = &ct.content else {
        panic!("r's content is a choice");
    };
    assert_eq!(branches.len(), 2);
    for branch in branches {
        let ElementRef::Group(group) = &branch.element else {
            panic!("a branch is not a group");
        };
        assert!(matches!(group.as_ref(), ContentModel::Sequence(p) if p.len() == 2));
    }
}

#[test]
fn a_branch_sharing_a_name_with_another_validates_as_before() {
    // Read as branch groups, the shared `a` would select the second branch
    // too and report its `c` missing.
    assert_eq!(errors("<r><a/><b/></r>"), Vec::<String>::new());
    assert_eq!(errors("<r><a/><c/></r>"), Vec::<String>::new());
    assert_eq!(
        errors("<r/>"),
        ["choice requires exactly one branch, none present"]
    );
}
