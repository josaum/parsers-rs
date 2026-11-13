//! Parts that are not UTF-8 read the same as they did with quick-xml 0.41.
//!
//! quick-xml 0.42 rejects a whole document on its first byte that is not
//! UTF-8. These pin what the parsers did before that: a property value reads
//! such a byte as U+FFFD, a relationship rejects it where it reads it, and a
//! byte in a place nothing reads costs nothing.

use std::collections::HashMap;

use ooxml_archive::{parse_relationships, parse_simple_props};

fn utf16le(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[test]
fn latin_1_byte_in_a_property_value_reads_as_replacement_character() {
    let xml = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\
        <cp:coreProperties xmlns:cp=\"urn:cp\" xmlns:dc=\"urn:dc\">\
        <dc:title>Caf\xe9 &amp; bar</dc:title>\
        <dc:creator x=\"\xe9\">Jos\xe9</dc:creator>\
        <dc:subject>plain</dc:subject>\
        </cp:coreProperties>";
    let props = parse_simple_props(xml).expect("main read this part");
    let expected: HashMap<String, String> = [
        ("title", "Caf\u{FFFD} & bar"),
        ("creator", "Jos\u{FFFD}"),
        ("subject", "plain"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(props, expected);
}

#[test]
fn latin_1_byte_in_a_property_name_reads_as_replacement_character() {
    let xml = b"<root><caf\xe9>value</caf\xe9></root>";
    let props = parse_simple_props(xml).expect("main read this part");
    assert_eq!(props.get("caf\u{FFFD}").map(String::as_str), Some("value"));
}

#[test]
fn latin_1_byte_in_a_relationship_attribute_that_is_read_is_an_error() {
    let xml = b"<?xml version=\"1.0\" encoding=\"windows-1252\"?>\
        <Relationships xmlns=\"urn:r\">\
        <Relationship Id=\"rId1\" Type=\"urn:t\" Target=\"caf\xe9.xml\"/>\
        </Relationships>";
    assert!(parse_relationships(xml).is_err());
}

#[test]
fn latin_1_byte_where_relationships_read_nothing_costs_nothing() {
    let xml = b"<?xml version=\"1.0\" encoding=\"windows-1252\"?>\
        <Relationships xmlns=\"urn:r\">\
        <!-- caf\xe9 -->\
        <Relationship Id=\"rId1\" Type=\"urn:t\" Target=\"a.xml\" Note=\"\xe9\"/>\
        <Other>caf\xe9</Other>\
        </Relationships>";
    let rels = parse_relationships(xml).expect("main read this part");
    assert_eq!(rels.entries.len(), 1);
    assert_eq!(rels.entries[0].id, "rId1");
    assert_eq!(rels.entries[0].rel_type, "urn:t");
    assert_eq!(rels.entries[0].target, "a.xml");
}

#[test]
fn utf16_property_part_reads_as_before() {
    // The properties parser is documented to take UTF-8; a UTF-16 part is
    // the caller's to transcode. Pin what an untranscoded one gives.
    let xml =
        utf16le("<?xml version=\"1.0\" encoding=\"UTF-16\"?><root><title>t\u{e9}</title></root>");
    let outcome = parse_simple_props(&xml).map(|props| {
        let mut pairs: Vec<(String, String)> = props.into_iter().collect();
        pairs.sort();
        pairs
    });
    // The UTF-16 bytes are read as if they were UTF-8: every other character
    // is NUL and the non-ASCII one is U+FFFD. Garbage, but the same garbage.
    let expected = vec![(
        "\0r\0o\0o\0t\0".to_owned(),
        "\0\n\0t\0\u{FFFD}\0\n\0\n\0".to_owned(),
    )];
    assert_eq!(outcome.expect("main read this part"), expected);
}

#[test]
fn real_replacement_character_in_a_target_survives_a_stray_byte_elsewhere() {
    // A U+FFFD the part really holds is a character like any other; a byte
    // that is not UTF-8 in a comment before or after it changes nothing.
    let rel = "<Relationship Id=\"rId1\" Type=\"urn:t\" \
        Target=\"http://e.com/a\u{FFFD}b\" TargetMode=\"External\"/>";
    for (before, after) in [("", ""), ("<!-- X -->", ""), ("", "<!-- X -->")] {
        let mut xml =
            format!("<Relationships xmlns=\"urn:r\">{before}{rel}{after}</Relationships>")
                .into_bytes();
        if let Some(at) = xml.iter().position(|&b| b == b'X') {
            xml[at] = 0xFF;
        }
        let rels = parse_relationships(&xml).expect("main read this part");
        assert_eq!(rels.entries.len(), 1);
        assert_eq!(rels.entries[0].target, "http://e.com/a\u{FFFD}b");
    }
}

#[test]
fn stray_byte_beside_a_real_replacement_character_in_one_target_is_an_error() {
    let xml = b"<Relationships xmlns=\"urn:r\">\
        <Relationship Id=\"rId1\" Type=\"urn:t\" Target=\"a\xef\xbf\xbd\xff\"/>\
        </Relationships>";
    assert!(parse_relationships(xml).is_err());
}
