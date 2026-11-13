//! Entity and character references in relationship attributes.
//!
//! The same input is planted in each attribute `parse_relationships` reads
//! and the outcome is compared against a table. The table is the contract: an
//! XML library upgrade must leave it unchanged, or change it on purpose in
//! the same commit.

use ooxml_archive::parse_relationships;

/// Replaces every `@` with the byte 0xE9, which is not valid UTF-8 on its own.
fn with_bad_byte(xml: &str) -> Vec<u8> {
    xml.bytes()
        .map(|byte| if byte == b'@' { 0xE9 } else { byte })
        .collect()
}

/// Reads `input` back from the `Target`, `Id` and `Type` attributes in turn.
fn attribute_paths(input: &str) -> Vec<(&'static str, String)> {
    let read = |id: &str,
                rel_type: &str,
                target: &str,
                pick: fn(&ooxml_archive::RelEntry) -> &str| {
        let xml = format!(
            r#"<Relationships><Relationship Id="{id}" Type="{rel_type}" Target="{target}"/></Relationships>"#
        );
        match parse_relationships(&with_bad_byte(&xml)) {
            Ok(rels) => rels
                .entries
                .first()
                .map_or_else(|| "<absent>".to_string(), |entry| pick(entry).to_string()),
            Err(_) => "<parse error>".to_string(),
        }
    };
    vec![
        ("Target", read("rId1", "t", input, |entry| &entry.target)),
        ("Id", read(input, "t", "x.xml", |entry| &entry.id)),
        (
            "Type",
            read("rId1", input, "x.xml", |entry| &entry.rel_type),
        ),
    ]
}

/// Renders the table: an input every attribute agrees on takes one line.
fn table(inputs: &[&str]) -> String {
    let mut out = String::new();
    for input in inputs {
        let results = attribute_paths(input);
        let first = &results[0].1;
        if results.iter().all(|(_, value)| value == first) {
            out.push_str(&format!("{input:?} => {first:?}\n"));
        } else {
            out.push_str(&format!("{input:?} =>\n"));
            for (path, value) in &results {
                out.push_str(&format!("    {path}: {value:?}\n"));
            }
        }
    }
    out
}

const ATTRIBUTE_INPUTS: &[&str] = &[
    "a &amp; b.xml",
    "&lt;x&gt; &quot;q&quot; &apos;s&apos;",
    "caf&#233;.xml",
    "&#x1F600;",
    " &amp; ",
    // Whitespace written literally and as character references.
    "a\tb",
    "a\nb",
    "a\r\nb",
    "a&#9;b&#10;c",
    "a &foo; b",
    "a&#0;b",
    "AT&T",
    "caf@",
];

#[test]
fn references_in_relationship_attributes() {
    let actual = table(ATTRIBUTE_INPUTS);
    let expected: String = r##"
"a &amp; b.xml" => "a & b.xml"
"&lt;x&gt; &quot;q&quot; &apos;s&apos;" => "<x> \"q\" 's'"
"caf&#233;.xml" => "café.xml"
"&#x1F600;" => "😀"
" &amp; " => " & "
"a\tb" => "a b"
"a\nb" => "a b"
"a\r\nb" => "a b"
"a&#9;b&#10;c" => "a\tb\nc"
"a &foo; b" => "<parse error>"
"a&#0;b" => "<parse error>"
"AT&T" => "<parse error>"
"caf@" => "<parse error>"
"##
    .lines()
    .skip_while(|line| line.is_empty())
    .map(|line| format!("{line}\n"))
    .collect();
    assert!(
        actual == expected,
        "the table changed.\n--- actual ---\n{actual}--- expected ---\n{expected}"
    );
}

/// Text between the elements of a `.rels` part is never read, so nothing in
/// it may cost the relationships: not a bare `&`, not an unknown entity.
#[test]
fn text_between_relationships_is_ignored() {
    for filler in ["\n  ", "AT&T", "&foo;", "&#0;", "<![CDATA[x]]>", "&amp"] {
        let xml = format!(
            r#"<Relationships>{filler}<Relationship Id="rId1" Type="t" Target="a.xml"/>{filler}</Relationships>"#
        );
        let rels = parse_relationships(xml.as_bytes())
            .unwrap_or_else(|err| panic!("filler {filler:?}: {err:#}"));
        assert_eq!(rels.entries.len(), 1, "filler {filler:?}");
        assert_eq!(rels.by_id.get("rId1").map(String::as_str), Some("a.xml"));
    }
}
