//! xsd-fast-node: N-API bindings mirroring `xsd-fast-py` (full parity).
use napi::bindgen_prelude::*;
use napi_derive::napi;
use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

fn map_err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

/// List all global element names in the schema.
/// Follows xs:include / xs:import relative paths automatically.
#[napi]
pub fn list_elements(schema_path: String, max_includes: Option<u32>) -> Result<Vec<String>> {
    let opts = ParseOptions {
        max_includes: max_includes.unwrap_or(64) as usize,
        ..Default::default()
    };
    let schema = SchemaSet::load(&schema_path, opts).map_err(map_err)?;
    Ok(schema.model().global_elements.keys().cloned().collect())
}

/// Return Arrow schema description for a root element as a JSON string.
#[napi]
pub fn describe_element_json(
    schema_path: String,
    element: String,
    max_depth: Option<u32>,
    map_dates: Option<bool>,
    include_attributes: Option<bool>,
) -> Result<String> {
    let parse_opts = ParseOptions::default();
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8) as usize,
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: include_attributes.unwrap_or(true),
        batch_size: 8_192,
    };
    let schema = SchemaSet::load(&schema_path, parse_opts).map_err(map_err)?;
    let arrow_schema = schema
        .to_arrow_schema(&element, arrow_opts)
        .map_err(map_err)?;
    let fields: Vec<serde_json::Value> = arrow_schema
        .fields()
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.name(),
                "arrow_type": format!("{:?}", f.data_type()),
                "nullable": f.is_nullable(),
            })
        })
        .collect();
    serde_json::to_string(&fields).map_err(map_err)
}

/// Validate an XML file against the schema. Returns JSON string.
#[napi]
pub fn validate_json(
    schema_path: String,
    xml_path: String,
    max_errors: Option<u32>,
    check_facets: Option<bool>,
    check_cardinality: Option<bool>,
) -> Result<String> {
    let validate_opts = ValidateOptions {
        max_errors: max_errors.unwrap_or(100) as usize,
        check_facets: check_facets.unwrap_or(true),
        check_cardinality: check_cardinality.unwrap_or(true),
    };
    let xml = std::fs::read(xml_path).map_err(map_err)?;
    let schema = SchemaSet::load(schema_path, ParseOptions::default()).map_err(map_err)?;
    let errors = schema.validate(&xml, validate_opts).map_err(map_err)?;
    serde_json::to_string(&errors).map_err(map_err)
}

/// Validate XML bytes against the schema. Returns JSON string.
#[napi]
pub fn validate_bytes_json(
    schema_path: String,
    xml_bytes: Buffer,
    max_errors: Option<u32>,
    check_facets: Option<bool>,
    check_cardinality: Option<bool>,
) -> Result<String> {
    let validate_opts = ValidateOptions {
        max_errors: max_errors.unwrap_or(100) as usize,
        check_facets: check_facets.unwrap_or(true),
        check_cardinality: check_cardinality.unwrap_or(true),
    };
    let schema = SchemaSet::load(schema_path, ParseOptions::default()).map_err(map_err)?;
    let errors = schema
        .validate(&xml_bytes, validate_opts)
        .map_err(map_err)?;
    serde_json::to_string(&errors).map_err(map_err)
}

/// Write Arrow IPC schema description for a root element to a file path.
#[napi]
pub fn write_schema_ipc(
    schema_path: String,
    element: String,
    out_path: String,
    max_depth: Option<u32>,
    map_dates: Option<bool>,
) -> Result<()> {
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8) as usize,
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: true,
        batch_size: 8_192,
    };
    let schema = SchemaSet::load(schema_path, ParseOptions::default()).map_err(map_err)?;
    let out = std::fs::File::create(out_path).map_err(map_err)?;
    schema
        .write_schema_ipc(&element, arrow_opts, std::io::BufWriter::new(out))
        .map_err(map_err)
}

/// Return Arrow IPC schema description bytes for a root element.
#[napi]
pub fn schema_ipc_bytes(
    schema_path: String,
    element: String,
    max_depth: Option<u32>,
    map_dates: Option<bool>,
) -> Result<Buffer> {
    let arrow_opts = ArrowOptions {
        max_depth: max_depth.unwrap_or(8) as usize,
        map_date_types: map_dates.unwrap_or(true),
        include_attributes: true,
        batch_size: 8_192,
    };
    let schema = SchemaSet::load(schema_path, ParseOptions::default()).map_err(map_err)?;
    let mut buf = Vec::new();
    schema
        .write_schema_ipc(&element, arrow_opts, &mut buf)
        .map_err(map_err)?;
    Ok(buf.into())
}

#[cfg(test)]
mod tests {
    //! Arrow IPC decode tests for schema descriptions emitted by `xsd-fast-core`.
    use super::*;
    use arrow::array::{Array, StringArray};
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;

    const NOTE_XSD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="note">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="to" type="xs:string"/>
        <xs:element name="body" type="xs:string"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>
"#;

    #[test]
    fn schema_ipc_bytes_decode_to_expected_description_rows() {
        let path = std::env::temp_dir().join(format!(
            "xsd-fast-node-note-{}-{}.xsd",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::write(&path, NOTE_XSD).expect("write xsd");
        let bytes = (|| {
            let schema = SchemaSet::load(&path, ParseOptions::default()).map_err(map_err)?;
            let mut buf = Vec::new();
            schema
                .write_schema_ipc("note", ArrowOptions::default(), &mut buf)
                .map_err(map_err)?;
            Ok::<Vec<u8>, Error>(buf)
        })();
        std::fs::remove_file(&path).ok();
        let bytes = bytes.expect("schema ipc bytes");

        assert_eq!(
            &bytes[0..4],
            &[0xff, 0xff, 0xff, 0xff],
            "missing Arrow IPC continuation marker"
        );
        let reader = StreamReader::try_new(Cursor::new(bytes), None).expect("stream reader");
        let schema = reader.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["path", "arrow_type", "nullable"]);
        let path_idx = schema.index_of("path").expect("path column");

        let mut paths = Vec::new();
        for maybe_batch in reader {
            let batch = maybe_batch.expect("batch");
            let path_col = batch
                .column(path_idx)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("path StringArray");
            for i in 0..path_col.len() {
                if !path_col.is_null(i) {
                    paths.push(path_col.value(i).to_string());
                }
            }
        }
        assert!(paths.contains(&"to".to_string()), "paths: {paths:?}");
        assert!(paths.contains(&"body".to_string()), "paths: {paths:?}");
    }
}
