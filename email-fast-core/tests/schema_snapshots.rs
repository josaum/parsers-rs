//! Golden Arrow schema snapshots for the email Arrow exporter.
//!
//! `tests/schema_snapshots/emails.snap` and `attachments.snap` freeze the
//! current schema reality (field names, `DataType`s, nullability, field order)
//! of `arrow::emails_schema()` and `arrow::attachments_schema()`. Any
//! intentional schema change — e.g. a future `DataType` migration — must update
//! the snapshot in the same commit so the change shows up as an explicit,
//! reviewable diff. Tests are compare-only: on mismatch the panic prints the
//! full actual rendering and the snapshot path so updating is copy-paste.

#![cfg(feature = "arrow")]

use arrow_schema::Schema;
use std::path::PathBuf;

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
fn email_schemas_match_golden_snapshots() {
    // WHY: the attachments schema used to be an unnamed inline literal inside
    // the IPC exporter; the accessor freezes it alongside the emails schema.
    assert_schema_snapshot("emails", &email_fast_core::arrow::emails_schema());
    assert_schema_snapshot("attachments", &email_fast_core::arrow::attachments_schema());
}
