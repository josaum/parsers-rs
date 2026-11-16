use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, ValidationSeverity};

fn schema(body: &str) -> SchemaSet {
    let text =
        format!(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">{body}</xs:schema>"#);
    SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default()).unwrap()
}

fn invalid(schema: &SchemaSet, xml: &str) -> bool {
    schema
        .validate(xml.as_bytes(), ValidateOptions::default())
        .unwrap()
        .iter()
        .any(|finding| finding.severity == ValidationSeverity::Error)
}

#[test]
fn rejects_incomplete_or_multiple_root_documents() {
    let s = schema(r#"<xs:element name="root" type="xs:string"/>"#);
    for xml in [
        "",
        " ",
        "<root>",
        "<root/><root/>",
        "text<root/>",
        "<root/>text",
        "<root>&missing;</root>",
        "<![CDATA[x]]><root/>",
    ] {
        assert!(invalid(&s, xml), "accepted {xml:?}");
    }
    assert!(!invalid(&s, "<!-- before --><root/><!-- after -->"));
}

#[test]
fn empty_elements_enforce_required_children() {
    let s = schema(
        r#"<xs:element name="root"><xs:complexType><xs:sequence>
        <xs:element name="child" type="xs:string"/>
    </xs:sequence></xs:complexType></xs:element>"#,
    );
    for xml in ["<root/>", "<root></root>"] {
        assert!(invalid(&s, xml), "accepted {xml}");
    }
    assert!(!invalid(&s, "<root><child/></root>"));
}

#[test]
fn occurrence_minimum_is_not_just_presence() {
    let s = schema(
        r#"<xs:element name="root"><xs:complexType><xs:sequence>
        <xs:element name="child" type="xs:string" minOccurs="2" maxOccurs="3"/>
    </xs:sequence></xs:complexType></xs:element>"#,
    );
    assert!(invalid(&s, "<root><child/></root>"));
    assert!(!invalid(&s, "<root><child/><child/></root>"));
}

#[test]
fn error_budget_is_enforced_within_one_element() {
    let s = schema(
        r#"<xs:element name="root"><xs:complexType><xs:sequence>
        <xs:element name="a" type="xs:string"/><xs:element name="b" type="xs:string"/>
        <xs:element name="c" type="xs:string"/>
    </xs:sequence></xs:complexType></xs:element>"#,
    );
    for max_errors in [1, 2] {
        let findings = s
            .validate(
                b"<root></root>",
                ValidateOptions {
                    max_errors,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(findings.len(), max_errors);
    }
    assert!(s
        .validate(
            b"<root/>",
            ValidateOptions {
                max_errors: 0,
                ..Default::default()
            }
        )
        .is_err());
}

#[test]
fn preserves_string_whitespace_and_cdata_text() {
    let s = schema(
        r#"<xs:element name="root"><xs:simpleType><xs:restriction base="xs:string">
        <xs:enumeration value=" a b "/>
    </xs:restriction></xs:simpleType></xs:element>"#,
    );
    for xml in [
        "<root> a b </root>",
        "<root> a<![CDATA[ b ]]></root>",
        "<root> a<!-- comment --> b </root>",
    ] {
        assert!(!invalid(&s, xml), "rejected {xml}");
    }
    assert!(invalid(&s, "<root><![CDATA[wrong]]></root>"));
}

#[test]
fn normalizes_whitespace_by_builtin_type() {
    let token = schema(
        r#"<xs:element name="root"><xs:simpleType><xs:restriction base="xs:token">
        <xs:enumeration value="a b"/>
    </xs:restriction></xs:simpleType></xs:element>"#,
    );
    assert!(!invalid(&token, "<root> \ta\n b \t</root>"));
    let normalized = schema(
        r#"<xs:element name="root"><xs:simpleType><xs:restriction base="xs:normalizedString">
        <xs:enumeration value=" a  b "/>
    </xs:restriction></xs:simpleType></xs:element>"#,
    );
    assert!(!invalid(&normalized, "<root>\ta\t\nb\t</root>"));
}

#[test]
fn unknown_root_and_child_are_errors() {
    let s = schema(r#"<xs:element name="root" type="xs:string"/>"#);
    assert!(invalid(&s, "<unknown/>"));
    assert!(invalid(&s, "<root><unknown/></root>"));
}
