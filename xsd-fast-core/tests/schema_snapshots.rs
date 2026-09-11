//! Golden Arrow schema snapshot for XSD-derived Arrow schemas.
//!
//! `tests/schema_snapshots/inline_fixture.snap` freezes the current schema
//! reality (field names, `DataType`s, nullability, field order) produced by
//! `SchemaSet::to_arrow_schema` with default `ArrowOptions` for the inline XSD
//! fixture below — which exercises `xs:date` → Date32, `xs:dateTime` →
//! Timestamp, attributes (`@`-prefixed columns), and nested complex elements
//! (nested `Struct` columns). Any intentional derivation change must update the
//! snapshot in the same commit so the change shows up as an explicit,
//! reviewable diff. The test is compare-only: on mismatch the panic prints the
//! full actual rendering and the snapshot path so updating is copy-paste.

#![cfg(feature = "arrow")]

use arrow_schema::Schema;
use std::path::PathBuf;
use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet};

/// Inline fixture covering date/dateTime mapping, attributes, and nesting.
const INLINE_XSD: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">

  <xs:element name="contrato">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="codigo" type="xs:string"/>
        <xs:element name="dataInicio" type="xs:date"/>
        <xs:element name="registradoEm" type="xs:dateTime"/>
        <xs:element name="ativo" type="xs:boolean"/>
        <xs:element name="valor" type="xs:decimal"/>
        <xs:element name="responsavel">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="nome" type="xs:string"/>
              <xs:element name="email" type="xs:string" minOccurs="0"/>
            </xs:sequence>
            <xs:attribute name="id" type="xs:string" use="required"/>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
      <xs:attribute name="versao" type="xs:string"/>
    </xs:complexType>
  </xs:element>

</xs:schema>
"#;

/// One line per field: `name: DataType(nullable=true|false)`, in schema order.
fn render_schema(schema: &Schema) -> String {
    let mut out = String::new();
    for field in schema.fields() {
        out.push_str(&format!(
            "{}: {:?}(nullable={})\n",
            field.name(),
            field.data_type(),
            field.is_nullable()
        ));
    }
    out
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/schema_snapshots")
        .join(format!("{name}.snap"))
}

fn assert_schema_snapshot(name: &str, schema: &Schema) {
    let actual = render_schema(schema);
    let path = snapshot_path(name);
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read snapshot `{}`: {err}\n\nactual rendering:\n{actual}",
            path.display()
        )
    });
    if expected != actual {
        panic!(
            "schema snapshot mismatch for `{name}`\nsnapshot path: {}\n\n--- actual rendering ---\n{actual}\n--- end ---\n\nTo update: overwrite the snapshot file with the actual rendering above and review the diff.",
            path.display()
        );
    }
}

#[test]
fn derived_schema_matches_golden_snapshot() {
    // WHY: default ArrowOptions map xs:date → Date32 and xs:dateTime →
    // Timestamp(Microsecond, UTC); freezing the derived schema makes any
    // future derivation change an explicit snapshot diff.
    let schema_set =
        SchemaSet::load_bytes(INLINE_XSD, ParseOptions::default()).expect("parse inline XSD");
    let schema = schema_set
        .to_arrow_schema("contrato", ArrowOptions::default())
        .expect("to_arrow_schema failed");
    assert_schema_snapshot("inline_fixture", &schema);
}
