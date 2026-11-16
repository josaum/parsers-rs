//! Documents that are not UTF-8 read as they did before quick-xml 0.42.
//!
//! quick-xml 0.42 refuses a whole document at its first byte that is not
//! UTF-8. The parser and the validator read such a byte as U+FFFD instead,
//! and keep treating a name, value or text holding one as unreadable, as they
//! did when the reader handed them raw bytes. The expected values below were
//! taken from the parser on quick-xml 0.41 and must not change.

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// An ISO-8859-1 schema: Latin-1 bytes in documentation, which is skipped,
/// and in an enumeration value and a type name, which are read.
const LATIN1_SCHEMA: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>
<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\" elementFormDefault=\"qualified\">
  <xs:annotation><xs:documentation>Descri\xe7\xe3o do esquema</xs:documentation></xs:annotation>
  <xs:simpleType name=\"resposta\">
    <xs:restriction base=\"xs:string\">
      <xs:enumeration value=\"Sim\"/>
      <xs:enumeration value=\"N\xe3o\"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name=\"c\xf3digo\">
    <xs:restriction base=\"xs:string\"><xs:maxLength value=\"3\"/></xs:restriction>
  </xs:simpleType>
  <xs:element name=\"r\">
    <xs:complexType>
      <xs:sequence>
        <xs:element name=\"v\" type=\"resposta\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>
        <xs:element name=\"n\" type=\"xs:string\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>
";

/// An ISO-8859-1 instance: Latin-1 bytes in element text, in an attribute
/// value with a reference, and in an element name.
const LATIN1_INSTANCE: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>
<r><v>Sim</v><v>N\xe3o</v><n a=\"\xe9\">x</n><n b=\"\xe9&amp;\">y</n><\xe9l/></r>
";

/// The schema model and the findings for the instance, as JSON.
fn read(schema: &[u8], instance: &[u8]) -> String {
    let schema = match SchemaSet::load_bytes(schema, ParseOptions::default()) {
        Ok(schema) => schema,
        Err(error) => return format!("schema error: {error}"),
    };
    let model = serde_json::to_string(schema.model()).expect("model serializes");
    let findings = match schema.validate(instance, ValidateOptions::default()) {
        Ok(findings) => serde_json::to_string(&findings).expect("findings serialize"),
        Err(error) => format!("validation error: {error}"),
    };
    format!("{model}\n{findings}")
}

#[test]
fn latin_1_schema_and_instance_read_as_before() {
    assert_eq!(read(LATIN1_SCHEMA, LATIN1_INSTANCE), EXPECTED_LATIN1);
}

#[test]
fn utf_16_schema_reads_as_before() {
    let text = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
        <xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\
        <xs:element name=\"r\" type=\"xs:string\"/></xs:schema>\n";
    let mut schema = vec![0xFF, 0xFE];
    schema.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(read(&schema, b"<r/>"), EXPECTED_UTF16);
}

#[test]
fn schema_location_that_is_not_utf_8_is_refused_as_before() {
    let schema = b"<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\
        <xs:include schemaLocation=\"c\xf3digo.xsd\"/></xs:schema>";
    // On quick-xml 0.41 the message went on with the reader's UTF-8 error:
    // schema error: invalid schemaLocation: cannot decode input using UTF-8: invalid utf-8 sequence of 1 bytes from index 1
    // The repaired value no longer holds the bytes that error described.
    let read = read(schema, b"<r/>");
    assert!(read.starts_with(REFUSED_SCHEMA_LOCATION), "{read}");
}

const EXPECTED_LATIN1: &str = "{\"target_namespace\":null,\"complex_types\":{},\"simple_types\":{\"resposta\":{\"name\":{\"namespace\":null,\"local\":\"resposta\"},\"kind\":\"String\",\"facets\":{\"enumerations\":[{\"value\":\"Sim\",\"documentation\":null},{\"value\":\"\",\"documentation\":null}],\"pattern\":null,\"min_length\":null,\"max_length\":null,\"min_inclusive\":null,\"max_inclusive\":null,\"min_exclusive\":null,\"max_exclusive\":null,\"total_digits\":null,\"fraction_digits\":null}}},\"global_elements\":{\"r\":{\"name\":{\"namespace\":null,\"local\":\"r\"},\"type_def\":{\"Complex\":{\"name\":null,\"content\":{\"Sequence\":[{\"occurrence\":{\"min\":0,\"max\":null},\"element\":{\"Inline\":{\"name\":{\"namespace\":null,\"local\":\"v\"},\"type_def\":{\"Ref\":{\"namespace\":null,\"local\":\"resposta\"}},\"occurrence\":{\"min\":0,\"max\":null},\"nillable\":false,\"documentation\":null}}},{\"occurrence\":{\"min\":0,\"max\":null},\"element\":{\"Inline\":{\"name\":{\"namespace\":null,\"local\":\"n\"},\"type_def\":{\"Simple\":{\"name\":null,\"kind\":\"String\",\"facets\":{\"enumerations\":[],\"pattern\":null,\"min_length\":null,\"max_length\":null,\"min_inclusive\":null,\"max_inclusive\":null,\"min_exclusive\":null,\"max_exclusive\":null,\"total_digits\":null,\"fraction_digits\":null}}},\"occurrence\":{\"min\":0,\"max\":null},\"nillable\":false,\"documentation\":null}}}]},\"attributes\":[],\"base\":null,\"derivation\":\"Extension\",\"prohibited_attributes\":[],\"empty_in_source\":false,\"content_occurrence\":{\"min\":1,\"max\":1}}},\"occurrence\":{\"min\":1,\"max\":1},\"nillable\":false,\"documentation\":null}}}\n[{\"severity\":\"Error\",\"path\":\"/r/n\",\"message\":\"XML parse error: ill-formed reference in an attribute value: a bare `&`, a character reference to no character, or an entity the document does not declare\"}]";
const EXPECTED_UTF16: &str = "{\"target_namespace\":null,\"complex_types\":{},\"simple_types\":{},\"global_elements\":{}}\n[{\"severity\":\"Error\",\"path\":\"/r\",\"message\":\"root element <r> is not declared in the schema\"}]";
const REFUSED_SCHEMA_LOCATION: &str = "schema error: invalid schemaLocation: ";
