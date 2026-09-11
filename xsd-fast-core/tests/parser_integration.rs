//! Integration tests for xsd-fast-core parser, validator, and arrow modules.

use std::fs;
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

// ── Inline XSD fixtures ───────────────────────────────────────────────────────

const SIMPLE_XSD: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test"
           elementFormDefault="qualified">

  <xs:simpleType name="st_codigo">
    <xs:restriction base="xs:string">
      <xs:minLength value="1"/>
      <xs:maxLength value="10"/>
    </xs:restriction>
  </xs:simpleType>

  <xs:simpleType name="st_status">
    <xs:restriction base="xs:string">
      <xs:enumeration value="ATIVO"/>
      <xs:enumeration value="INATIVO"/>
      <xs:enumeration value="PENDENTE"/>
    </xs:restriction>
  </xs:simpleType>

  <xs:complexType name="ct_beneficiario">
    <xs:sequence>
      <xs:element name="codigo" type="st_codigo"/>
      <xs:element name="status" type="st_status"/>
      <xs:element name="nascimento" type="xs:date" minOccurs="0"/>
      <xs:element name="valor" type="xs:decimal" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="registroANS" type="xs:string" use="required"/>
  </xs:complexType>

  <xs:element name="beneficiario" type="ct_beneficiario"/>

  <xs:element name="mensagem">
    <xs:complexType>
      <xs:sequence>
        <xs:element ref="beneficiario" minOccurs="1" maxOccurs="unbounded"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>

</xs:schema>
"#;

const VALID_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<mensagem xmlns="urn:test">
  <beneficiario registroANS="123456">
    <codigo>ABC123</codigo>
    <status>ATIVO</status>
    <nascimento>1985-06-15</nascimento>
    <valor>1234.56</valor>
  </beneficiario>
</mensagem>
"#;

const INVALID_ENUM_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<mensagem xmlns="urn:test">
  <beneficiario registroANS="123456">
    <codigo>ABC123</codigo>
    <status>INEXISTENTE</status>
  </beneficiario>
</mensagem>
"#;

const INVALID_LENGTH_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<mensagem xmlns="urn:test">
  <beneficiario registroANS="123456">
    <codigo>TOOLONGVALUE123456</codigo>
    <status>ATIVO</status>
  </beneficiario>
</mensagem>
"#;

// ── Parser tests ──────────────────────────────────────────────────────────────

#[test]
fn parse_inline_xsd_global_elements() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let model = schema.model();
    assert!(
        model.global_elements.contains_key("beneficiario"),
        "expected global element 'beneficiario'"
    );
    assert!(
        model.global_elements.contains_key("mensagem"),
        "expected global element 'mensagem'"
    );
}

#[test]
fn parse_named_complex_type() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let model = schema.model();
    assert!(
        model.complex_types.contains_key("ct_beneficiario"),
        "expected complex type 'ct_beneficiario'"
    );
}

#[test]
fn parse_named_simple_type_enum() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let model = schema.model();
    let st = model
        .simple_types
        .get("st_status")
        .expect("st_status not found");
    assert_eq!(st.facets.enumerations.len(), 3);
    let values: Vec<&str> = st
        .facets
        .enumerations
        .iter()
        .map(|e| e.value.as_str())
        .collect();
    assert!(values.contains(&"ATIVO"));
    assert!(values.contains(&"INATIVO"));
    assert!(values.contains(&"PENDENTE"));
}

#[test]
fn parse_simple_type_length_facets() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let model = schema.model();
    let st = model
        .simple_types
        .get("st_codigo")
        .expect("st_codigo not found");
    assert_eq!(st.facets.min_length, Some(1));
    assert_eq!(st.facets.max_length, Some(10));
}

#[test]
fn parse_target_namespace() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    assert_eq!(schema.model().target_namespace.as_deref(), Some("urn:test"));
}

#[test]
fn rejects_schema_location_that_escapes_uploaded_root() {
    let root = tempfile::tempdir().expect("temporary schema root");
    let outside = root.path().parent().expect("parent").join("outside.xsd");
    fs::write(&outside, SIMPLE_XSD).expect("outside fixture");
    let entry = root.path().join("entry.xsd");
    fs::write(
        &entry,
        br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:include schemaLocation="../outside.xsd"/></xs:schema>"#,
    )
    .expect("entry fixture");

    let err = match SchemaSet::load(&entry, ParseOptions::default()) {
        Ok(_) => panic!("an escaping schemaLocation must be rejected"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("rejected schemaLocation"),
        "{err:#}"
    );
}

#[test]
fn rejects_in_memory_schema_above_source_budget() {
    let options = ParseOptions {
        max_schema_bytes: SIMPLE_XSD.len() - 1,
        ..Default::default()
    };
    let err = match SchemaSet::load_bytes(SIMPLE_XSD, options) {
        Ok(_) => panic!("an oversized in-memory schema must be rejected"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("resource_limit: schema_bytes"),
        "{err:#}"
    );
}

#[test]
fn aggregate_schema_budget_includes_root_and_followed_files() {
    let root = tempfile::tempdir().expect("temporary schema root");
    let child = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:element name="child" type="xs:string"/></xs:schema>"#;
    let entry = br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:include schemaLocation="child.xsd"/><xs:element name="root" type="xs:string"/></xs:schema>"#;
    fs::write(root.path().join("child.xsd"), child).expect("child fixture");
    let entry_path = root.path().join("entry.xsd");
    fs::write(&entry_path, entry).expect("entry fixture");

    let options = ParseOptions {
        max_schema_bytes: entry.len() + child.len() - 1,
        ..Default::default()
    };
    let err = match SchemaSet::load(&entry_path, options) {
        Ok(_) => panic!("the aggregate root plus include budget must be enforced"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("resource_limit: schema_bytes"),
        "{err:#}"
    );
}

#[test]
fn include_budget_exhaustion_is_an_error_not_a_partial_schema() {
    let root = tempfile::tempdir().expect("temporary schema root");
    fs::write(root.path().join("child.xsd"), SIMPLE_XSD).expect("child fixture");
    let entry_path = root.path().join("entry.xsd");
    fs::write(
        &entry_path,
        br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:include schemaLocation="child.xsd"/></xs:schema>"#,
    )
    .expect("entry fixture");

    let options = ParseOptions {
        max_includes: 0,
        ..Default::default()
    };
    let err = match SchemaSet::load(&entry_path, options) {
        Ok(_) => panic!("exhausting the include budget must reject the schema set"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("resource_limit: schema_includes"),
        "{err:#}"
    );
}

#[test]
fn schema_location_is_bounded_before_string_allocation() {
    let root = tempfile::tempdir().expect("temporary schema root");
    let entry_path = root.path().join("entry.xsd");
    fs::write(
        &entry_path,
        br#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:include schemaLocation="a-very-long-child-name.xsd"/></xs:schema>"#,
    )
    .expect("entry fixture");

    let options = ParseOptions {
        max_schema_location_bytes: 8,
        ..Default::default()
    };
    let err = match SchemaSet::load(&entry_path, options) {
        Ok(_) => panic!("an overlong schemaLocation must be rejected before resolution"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("resource_limit: schema_location_bytes"),
        "{err:#}"
    );
}

// ── Validator tests ───────────────────────────────────────────────────────────

#[test]
fn validate_valid_xml_no_errors() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let errors = schema
        .validate(VALID_XML, ValidateOptions::default())
        .expect("validate failed");
    // Filter to errors only (ignore warnings).
    let errors_only: Vec<_> = errors
        .iter()
        .filter(|e| e.severity == xsd_fast_core::ValidationSeverity::Error)
        .collect();
    assert!(
        errors_only.is_empty(),
        "expected no errors but got: {errors_only:#?}"
    );
}

#[test]
fn validate_invalid_enum_produces_error() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let errors = schema
        .validate(INVALID_ENUM_XML, ValidateOptions::default())
        .expect("validate failed");
    let has_enum_error = errors.iter().any(|e| {
        e.severity == xsd_fast_core::ValidationSeverity::Error && e.message.contains("enumeration")
    });
    assert!(
        has_enum_error,
        "expected enumeration error, got: {errors:#?}"
    );
}

#[test]
fn validate_invalid_length_produces_error() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let errors = schema
        .validate(INVALID_LENGTH_XML, ValidateOptions::default())
        .expect("validate failed");
    let has_len_error = errors.iter().any(|e| {
        e.severity == xsd_fast_core::ValidationSeverity::Error && e.message.contains("maxLength")
    });
    assert!(has_len_error, "expected maxLength error, got: {errors:#?}");
}

#[test]
fn validate_max_errors_limit_respected() {
    let schema = SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
    let opts = ValidateOptions {
        max_errors: 1,
        ..Default::default()
    };
    let errors = schema
        .validate(INVALID_ENUM_XML, opts)
        .expect("validate failed");
    assert!(errors.len() <= 1, "expected at most 1 error");
}

// ── Arrow tests ───────────────────────────────────────────────────────────────

#[cfg(feature = "arrow")]
mod arrow_tests {
    use super::*;
    use arrow_schema::DataType;
    use xsd_fast_core::arrow::ArrowOptions;

    #[test]
    fn derive_arrow_schema_for_beneficiario() {
        let schema =
            SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
        let arrow = schema
            .to_arrow_schema("beneficiario", ArrowOptions::default())
            .expect("to_arrow_schema failed");

        // Should have columns for child elements.
        let names: Vec<&str> = arrow.fields().iter().map(|f| f.name().as_str()).collect();
        assert!(
            names.contains(&"codigo"),
            "missing 'codigo' field, got: {names:?}"
        );
        assert!(
            names.contains(&"status"),
            "missing 'status' field, got: {names:?}"
        );
        assert!(
            !arrow
                .field_with_name("codigo")
                .expect("no 'codigo' field")
                .is_nullable(),
            "xs:element defaults to minOccurs=1 and must be required"
        );
        assert!(
            arrow
                .field_with_name("nascimento")
                .expect("no 'nascimento' field")
                .is_nullable(),
            "minOccurs=0 must remain nullable"
        );
    }

    #[test]
    fn derive_arrow_schema_date_mapped() {
        let schema =
            SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
        let opts = ArrowOptions {
            map_date_types: true,
            ..Default::default()
        };
        let arrow = schema
            .to_arrow_schema("beneficiario", opts)
            .expect("to_arrow_schema failed");
        let nascimento = arrow
            .field_with_name("nascimento")
            .expect("no 'nascimento' field");
        assert_eq!(
            *nascimento.data_type(),
            DataType::Date32,
            "xs:date should map to Date32"
        );
    }

    #[test]
    fn derive_arrow_schema_date_not_mapped() {
        let schema =
            SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
        let opts = ArrowOptions {
            map_date_types: false,
            ..Default::default()
        };
        let arrow = schema
            .to_arrow_schema("beneficiario", opts)
            .expect("to_arrow_schema failed");
        let nascimento = arrow
            .field_with_name("nascimento")
            .expect("no 'nascimento' field");
        assert_eq!(
            *nascimento.data_type(),
            DataType::Utf8,
            "xs:date should stay Utf8 when map_date_types=false"
        );
    }

    #[test]
    fn derive_schema_description_batch() {
        let schema =
            SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
        let batch = schema
            .to_schema_description_batch("beneficiario", ArrowOptions::default())
            .expect("to_schema_description_batch failed");
        assert!(batch.num_rows() > 0, "description batch must not be empty");
        assert_eq!(batch.num_columns(), 3);
    }

    #[test]
    fn all_arrow_schemas_non_empty() {
        let schema =
            SchemaSet::load_bytes(SIMPLE_XSD, ParseOptions::default()).expect("parse failed");
        let all = schema.all_arrow_schemas(ArrowOptions::default());
        assert!(
            !all.is_empty(),
            "expected at least one Arrow schema from global elements"
        );
    }
}

// ── ANS-like multi-file XSD fixture (inline via include simulation) ───────────

/// Regression: a schema that has only attributes and empty content should produce
/// an Arrow schema with only attribute columns (prefixed @).
#[cfg(feature = "arrow")]
#[test]
fn arrow_schema_attributes_only() {
    use xsd_fast_core::arrow::ArrowOptions;

    let xsd = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="registro">
    <xs:complexType>
      <xs:attribute name="id" type="xs:string" use="required"/>
      <xs:attribute name="versao" type="xs:string"/>
    </xs:complexType>
  </xs:element>
</xs:schema>
"#;
    let schema = SchemaSet::load_bytes(xsd, ParseOptions::default()).expect("parse");
    let arrow = schema
        .to_arrow_schema("registro", ArrowOptions::default())
        .expect("arrow schema");
    let names: Vec<&str> = arrow.fields().iter().map(|f| f.name().as_str()).collect();
    assert!(
        names.contains(&"@id"),
        "expected '@id' attribute column, got: {names:?}"
    );
}
