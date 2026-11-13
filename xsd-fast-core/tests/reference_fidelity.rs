//! What the crate reads for each way of writing a character.
//!
//! The schema side reads attribute values only (enumeration values, patterns,
//! defaults, fixed values, names, the target namespace); it never reads
//! element text, so `xs:documentation` is skipped whatever it holds. The
//! instance side reads element text. Each case below is one spelling and the
//! value each side must make of it, so a change of XML reader cannot change
//! what is read without a test saying so.
//!
//! Attribute values are normalized as XML requires: a literal tab, newline or
//! CR LF pair is one space, while the same character written as a reference
//! is kept. Element text is not normalized. In element text, a reference that
//! stands for no character (an unknown entity, `&#0;`) and a bare `&` are kept
//! as written; an attribute holding one of those is dropped whole.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// One spelling of a value and what each reader makes of it.
struct Case {
    /// Short name used in failure messages.
    label: &'static str,
    /// The value as written in the XML source.
    written: &'static str,
    /// What a schema attribute holding it reads as; `None` when the whole
    /// attribute is dropped, as if it had not been written.
    attribute: Option<&'static str>,
    /// What an instance element holding it is validated as (after the
    /// surrounding whitespace is trimmed).
    text: &'static str,
}

const CASES: &[Case] = &[
    Case {
        label: "predefined entity between spaces",
        written: "a &amp; b",
        attribute: Some("a & b"),
        text: "a & b",
    },
    Case {
        label: "predefined entity",
        written: "x&lt;y",
        attribute: Some("x<y"),
        text: "x<y",
    },
    Case {
        label: "decimal character reference",
        written: "caf&#233;",
        attribute: Some("caf\u{e9}"),
        text: "caf\u{e9}",
    },
    Case {
        label: "hex character reference",
        written: "&#x1F600;",
        attribute: Some("\u{1F600}"),
        text: "\u{1F600}",
    },
    Case {
        label: "unknown entity",
        written: "a&foo;b",
        attribute: None,
        text: "a&foo;b",
    },
    Case {
        label: "character reference to NUL",
        written: "a&#0;b",
        attribute: None,
        text: "a&#0;b",
    },
    Case {
        label: "reference with only spaces around it",
        written: " &amp; ",
        attribute: Some(" & "),
        text: "&",
    },
    Case {
        label: "bare ampersand",
        written: "a & b",
        attribute: None,
        text: "a & b",
    },
    Case {
        label: "literal tab",
        written: "a\tb",
        attribute: Some("a b"),
        text: "a\tb",
    },
    Case {
        label: "literal newline",
        written: "a\nb",
        attribute: Some("a b"),
        text: "a\nb",
    },
    Case {
        label: "literal CR LF",
        written: "a\r\nb",
        attribute: Some("a b"),
        text: "a\r\nb",
    },
    Case {
        label: "tab as a character reference",
        written: "a&#9;b",
        attribute: Some("a\tb"),
        text: "a\tb",
    },
    Case {
        label: "newline as a character reference",
        written: "a&#10;b",
        attribute: Some("a\nb"),
        text: "a\nb",
    },
];

fn schema_document(attributes: &str, body: &str) -> String {
    format!(
        "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"{attributes}>{body}</xs:schema>"
    )
}

fn load(text: &str) -> SchemaSet {
    SchemaSet::load_bytes(text.as_bytes(), ParseOptions::default())
        .unwrap_or_else(|error| panic!("schema must load: {error:#}\n{text}"))
}

#[test]
fn facet_values_read_each_spelling() {
    for case in CASES {
        let written = case.written;
        let schema = load(&schema_document(
            "",
            &format!(
                "<xs:simpleType name=\"t\"><xs:restriction base=\"xs:string\">\
                 <xs:enumeration value=\"{written}\"/><xs:pattern value=\"{written}\"/>\
                 </xs:restriction></xs:simpleType>"
            ),
        ));
        let facets = &schema.model().simple_types["t"].facets;
        let values: Vec<&str> = facets
            .enumerations
            .iter()
            .map(|value| value.value.as_str())
            .collect();
        // A dropped `value` attribute leaves the facet with an empty value.
        let expected = case.attribute.unwrap_or("");
        assert_eq!(values, [expected], "enumeration: {}", case.label);
        assert_eq!(
            facets.pattern.as_deref(),
            Some(expected),
            "pattern: {}",
            case.label
        );
    }
}

#[test]
fn attribute_defaults_fixed_values_and_names_read_each_spelling() {
    for case in CASES {
        let written = case.written;
        let schema = load(&schema_document(
            "",
            &format!(
                "<xs:complexType name=\"c\"><xs:attribute name=\"a\" default=\"{written}\"/>\
                 <xs:attribute name=\"b\" fixed=\"{written}\"/>\
                 <xs:attribute name=\"{written}\"/></xs:complexType>"
            ),
        ));
        let attributes = &schema.model().complex_types["c"].attributes;
        let read: Vec<(&str, Option<&str>, Option<&str>)> = attributes
            .iter()
            .map(|a| (a.name.as_str(), a.default.as_deref(), a.fixed.as_deref()))
            .collect();
        let mut expected = vec![("a", case.attribute, None), ("b", None, case.attribute)];
        // An attribute declaration whose `name` is dropped is not declared.
        if let Some(name) = case.attribute {
            expected.push((name, None, None));
        }
        assert_eq!(read, expected, "{}", case.label);
    }
}

#[test]
fn declaration_names_and_target_namespace_read_each_spelling() {
    for case in CASES {
        let written = case.written;
        let schema = load(&schema_document(
            &format!(" targetNamespace=\"{written}\""),
            &format!(
                "<xs:element name=\"{written}\" type=\"xs:string\"/>\
                 <xs:simpleType name=\"{written}\"><xs:restriction base=\"xs:string\"/></xs:simpleType>\
                 <xs:complexType name=\"{written}\"><xs:sequence>\
                 <xs:element name=\"{written}\" type=\"xs:string\"/>\
                 </xs:sequence></xs:complexType>"
            ),
        ));
        let model = schema.model();
        let expected: Vec<&str> = case.attribute.into_iter().collect();
        assert_eq!(
            model.target_namespace.as_deref(),
            case.attribute,
            "targetNamespace: {}",
            case.label
        );
        let elements: Vec<&str> = model.global_elements.keys().map(String::as_str).collect();
        assert_eq!(elements, expected, "element name: {}", case.label);
        let simple: Vec<&str> = model.simple_types.keys().map(String::as_str).collect();
        assert_eq!(simple, expected, "simpleType name: {}", case.label);
        let complex: Vec<&str> = model.complex_types.keys().map(String::as_str).collect();
        assert_eq!(complex, expected, "complexType name: {}", case.label);
        if let Some(name) = case.attribute {
            let json = serde_json::to_string(&model.complex_types[name]).expect("serializes");
            let quoted = serde_json::to_string(name).expect("serializes");
            assert!(
                json.contains(&format!("\"local\":{quoted}")),
                "nested element name: {}: {json}",
                case.label
            );
        }
    }
}

#[test]
fn documentation_text_is_skipped_whatever_it_holds() {
    for case in CASES {
        let written = case.written;
        let schema = load(&schema_document(
            "",
            &format!(
                "<xs:annotation><xs:documentation>{written}<![CDATA[x]]>{written}\
                 </xs:documentation></xs:annotation>\
                 <xs:element name=\"e\" type=\"xs:string\"><xs:annotation>\
                 <xs:documentation> {written} </xs:documentation></xs:annotation></xs:element>\
                 <xs:element name=\"after\" type=\"xs:string\"/>"
            ),
        ));
        let model = schema.model();
        let elements: Vec<&str> = model.global_elements.keys().map(String::as_str).collect();
        assert_eq!(elements, ["e", "after"], "{}", case.label);
        assert_eq!(model.global_elements["e"].documentation, None);
    }
}

/// The finding for `<v>` holding `body`, whose only allowed value is `never`.
///
/// The message quotes the value the element was validated as.
fn finding_for(body: &str) -> String {
    let schema = load(&schema_document(
        "",
        "<xs:element name=\"r\"><xs:complexType><xs:sequence>\
         <xs:element name=\"v\"><xs:simpleType><xs:restriction base=\"xs:string\">\
         <xs:enumeration value=\"never\"/></xs:restriction></xs:simpleType></xs:element>\
         </xs:sequence></xs:complexType></xs:element>",
    ));
    let document = format!("<r><v>{body}</v></r>");
    let findings = schema
        .validate(document.as_bytes(), ValidateOptions::default())
        .expect("validation runs");
    // An ill-formed reference adds a parse-error finding of its own; the
    // value is still validated as written (see tests/ill_formed_references.rs).
    let mut values: Vec<String> = findings
        .into_iter()
        .map(|finding| finding.message)
        .filter(|message| !message.starts_with("XML parse error: "))
        .collect();
    assert_eq!(values.len(), 1, "{body:?}: {values:?}");
    values.remove(0)
}

/// The finding that reports `text` as the validated value.
fn refused(text: &str) -> String {
    format!("value {text:?} not in enumeration [never]")
}

#[test]
fn instance_text_reads_each_spelling() {
    for case in CASES {
        assert_eq!(
            finding_for(case.written),
            refused(case.text),
            "{}",
            case.label
        );
    }
}

#[test]
fn instance_text_keeps_references_next_to_cdata_and_spaces() {
    let cases = [
        ("&amp;<![CDATA[ b]]>", "& b"),
        ("<![CDATA[a ]]>&amp;", "a &"),
        (" a &lt; <![CDATA[b]]> &gt; c ", "a < b > c"),
        ("&foo;", "&foo;"),
        ("x &amp; &foo; y", "x & &foo; y"),
        ("a & b; c &", "a & b; c &"),
        ("&#xD800;&#x110000;&#;", "&#xD800;&#x110000;&#;"),
    ];
    for (written, text) in cases {
        assert_eq!(finding_for(written), refused(text), "{written:?}");
    }
}
