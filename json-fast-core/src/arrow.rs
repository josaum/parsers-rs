//! Columnar JSONL → Apache Arrow bridge.
//!
//! Gated behind the `arrow` feature. Parses a JSONL/NDJSON stream of objects
//! into [`RecordBatch`]es with a columnar schema, and optionally writes Arrow
//! IPC (Feather v2). This is the transport seam for the rest of the stack:
//! [`xlsx-fast-flight`] shows the flight-server pattern this feature feeds,
//! and [`json-fast-zerobus`] shows the Databricks Arrow-Flight ingest pattern.
//!
//! # Schema model
//!
//! JSON is schemaless; Arrow is schemaful. Two modes are supported:
//!
//! 1. **Inference** (default): the union of top-level keys across the stream is
//!    collected in first-seen order and each key is typed by unification over
//!    all records. Nested objects/arrays become UTF-8 JSON-string columns so no
//!    structure is lost. Numeric columns become `Int64` when every value is an
//!    integer, `Float64` otherwise.
//! 2. **Explicit** ([`ArrowOptions::schema`]): the caller supplies the schema.
//!    This is required by consumers that need a fixed contract — e.g.
//!    Databricks Zerobus requires the stream schema to match the target Delta
//!    table 1:1.
//!
//! Column types supported in v1: `Boolean`, `Int64`, `Float64`, `Utf8`.
//! Explicit schemas may additionally use `Timestamp(Microsecond, UTC)` (the
//! Atlas pin, matching a Databricks Delta `TIMESTAMP` column) or the naive
//! `Timestamp(Microsecond, None)` alias for `TIMESTAMP_NTZ`: values are
//! RFC-3339 / ISO-8601 strings, parsed to UTC epoch microseconds. Timestamps
//! are never *inferred* — a string column stays `Utf8` unless the caller
//! names it `Timestamp`, so inference cannot misclassify a date-like string.
//!
//! Every column is nullable, and non-object JSONL lines fail loud.
//!
//! The workspace's Flight and Zerobus crates consume this Arrow surface.

use crate::{JsonError, JsonLimits, JsonResource, Result};
use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use serde_json::Value;
use std::cell::Cell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

/// Columnar kind inferred for a JSON field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    /// Every value was a JSON boolean.
    Bool,
    /// Every value was an integer number.
    Int,
    /// At least one value was a non-integer number.
    Float,
    /// At least one value was a JSON string (or numbers/bools were mixed).
    Str,
    /// At least one value was a nested object or array → stored as JSON text.
    Json,
}

/// Unify two column kinds into the least-lossy common kind.
fn unify(a: ColKind, b: ColKind) -> ColKind {
    use ColKind::*;
    // Nested JSON text can represent anything — keep it as JSON text.
    if a == Json || b == Json {
        return Json;
    }
    if a == Str || b == Str {
        return Str;
    }
    match (a, b) {
        (Int, Float) | (Float, Int) => Float,
        (x, y) if x == y => x,
        // Bool mixed with numbers loses meaning as a numeric column.
        _ => Str,
    }
}

/// The column kind of a non-null JSON value.
fn value_kind(value: &Value) -> ColKind {
    match value {
        Value::Null => ColKind::Str, // unreachable: callers skip nulls
        Value::Bool(_) => ColKind::Bool,
        Value::Number(n) if n.as_i64().is_some() => ColKind::Int,
        Value::Number(_) => ColKind::Float,
        Value::String(_) => ColKind::Str,
        Value::Array(_) | Value::Object(_) => ColKind::Json,
    }
}

/// Map a column kind to its Arrow data type.
fn kind_to_data_type(kind: ColKind) -> DataType {
    match kind {
        ColKind::Bool => DataType::Boolean,
        ColKind::Int => DataType::Int64,
        ColKind::Float => DataType::Float64,
        ColKind::Str | ColKind::Json => DataType::Utf8,
    }
}

/// Options controlling JSONL → Arrow conversion.
#[derive(Debug, Clone)]
pub struct ArrowOptions {
    /// Number of records per emitted `RecordBatch` (default: 16_384).
    pub batch_size: usize,
    /// Optional fixed schema. When `None` (default), the schema is inferred
    /// from the union of top-level keys and their unified types.
    pub schema: Option<SchemaRef>,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self {
            batch_size: 16_384,
            schema: None,
        }
    }
}

/// Infer an Arrow schema from parsed JSONL records.
///
/// Keys are collected in first-seen order; each key is typed by unification
/// across all records (see module docs). Every field is nullable. Nested
/// objects/arrays become UTF-8 JSON-string columns.
///
/// Errors when any record is not a JSON object.
pub fn infer_schema(records: &[Value]) -> Result<SchemaRef> {
    let mut key_order: Vec<String> = Vec::new();
    let mut kinds: HashMap<String, ColKind> = HashMap::new();

    for record in records {
        let object = record.as_object().ok_or_else(|| {
            JsonError::Structure(
                "arrow conversion requires exactly one JSON object per line".to_string(),
            )
        })?;
        for (key, value) in object {
            if !key_order.iter().any(|k| k == key) {
                key_order.push(key.clone());
            }
            if value.is_null() {
                continue;
            }
            let kind = value_kind(value);
            kinds
                .entry(key.clone())
                .and_modify(|existing| *existing = unify(*existing, kind))
                .or_insert(kind);
        }
    }

    let fields = key_order
        .into_iter()
        .map(|key| {
            let kind = kinds.get(&key).copied().unwrap_or(ColKind::Str);
            Field::new(key, kind_to_data_type(kind), true)
        })
        .collect::<Vec<_>>();

    Ok(Arc::new(Schema::new(fields)))
}

/// The Arrow schema for a JSONL document, using the inference or explicit
/// schema from `options`.
pub fn jsonl_schema(jsonl: &str, options: &ArrowOptions) -> Result<SchemaRef> {
    jsonl_schema_with_limits(jsonl, options, &JsonLimits::default())
}

/// Resolve the Arrow schema for JSONL under explicit parse limits.
pub fn jsonl_schema_with_limits(
    jsonl: &str,
    options: &ArrowOptions,
    limits: &JsonLimits,
) -> Result<SchemaRef> {
    let records: Vec<Value> = crate::jsonl::parse_many_with_limits(jsonl, limits)?;
    match &options.schema {
        Some(schema) => Ok(schema.clone()),
        None => infer_schema(&records),
    }
}

/// Parse an RFC-3339 / ISO-8601 timestamp string to UTC epoch microseconds.
///
/// Pydantic's `datetime` serializes with an offset (`...Z` or `+00:00`), which
/// `chrono` handles. Offset timestamps are normalized to UTC.
fn parse_timestamp_micros(value: &str) -> Result<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value).map_err(|err| {
        JsonError::Structure(format!("invalid RFC-3339 timestamp {value:?}: {err}"))
    })?;
    Ok(parsed.timestamp_micros())
}

/// Look up `key` on a JSON object record. Missing keys and non-objects yield `None`.
fn field_value<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    record.as_object().and_then(|object| object.get(key))
}

/// Build one column array for `key` from a slice of records.
fn build_column(records: &[Value], key: &str, data_type: &DataType) -> Result<ArrayRef> {
    // Missing keys and JSON nulls both append a typed null.

    match data_type {
        DataType::Boolean => {
            let mut builder = BooleanBuilder::new();
            for record in records {
                builder.append_option(field_value(record, key).and_then(Value::as_bool));
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Int64 => {
            let mut builder = Int64Builder::new();
            for record in records {
                builder.append_option(field_value(record, key).and_then(Value::as_i64));
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Float64 => {
            let mut builder = Float64Builder::new();
            for record in records {
                builder.append_option(field_value(record, key).and_then(Value::as_f64));
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Timestamp(TimeUnit::Microsecond, tz) => {
            let mut builder = TimestampMicrosecondBuilder::new().with_timezone_opt(tz.clone());
            for record in records {
                match field_value(record, key) {
                    None | Some(Value::Null) => builder.append_option(None),
                    Some(Value::String(s)) => {
                        builder.append_value(parse_timestamp_micros(s)?);
                    }
                    Some(other) => {
                        return Err(JsonError::Structure(format!(
                            "timestamp column {key:?} expected an RFC-3339 string, got: {other}"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Utf8 => {
            let mut builder = StringBuilder::new();
            for record in records {
                match field_value(record, key) {
                    None | Some(Value::Null) => builder.append_null(),
                    Some(Value::String(s)) => builder.append_value(s.as_str()),
                    Some(Value::Bool(b)) => builder.append_value(if *b { "true" } else { "false" }),
                    Some(Value::Number(n)) => builder.append_value(n.to_string()),
                    Some(nested @ (Value::Array(_) | Value::Object(_))) => {
                        let text = serde_json::to_string(nested).map_err(JsonError::from)?;
                        builder.append_value(&text);
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        other => Err(JsonError::Structure(format!(
            "unsupported arrow column type for {key:?}: {other}"
        ))),
    }
}

/// Build a [`RecordBatch`] from a slice of parsed records against a fixed
/// schema. The schema must contain exactly the top-level keys to emit; unknown
/// schema types are rejected fail-loud.
pub fn build_batch(records: &[Value], schema: &SchemaRef) -> Result<RecordBatch> {
    let columns = schema
        .fields()
        .iter()
        .map(|field| build_column(records, field.name(), field.data_type()))
        .collect::<Result<Vec<_>>>()?;
    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|err| JsonError::Structure(format!("arrow batch construction failed: {err}")))
}

/// Stream a JSONL document into [`RecordBatch`]es via a callback.
///
/// Schema is inferred unless [`ArrowOptions::schema`] is set. Batch size is
/// controlled by [`ArrowOptions::batch_size`]. Returning an error from `emit`
/// halts conversion; errors inside are surfaced as [`JsonError`].
pub fn parse_jsonl_to_arrow_batches<F>(jsonl: &str, options: &ArrowOptions, emit: F) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    parse_jsonl_to_arrow_batches_with_limits(jsonl, options, &JsonLimits::default(), emit)
}

/// Stream bounded JSONL into Arrow batches via a callback.
pub fn parse_jsonl_to_arrow_batches_with_limits<F>(
    jsonl: &str,
    options: &ArrowOptions,
    limits: &JsonLimits,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let records: Vec<Value> = crate::jsonl::parse_many_with_limits(jsonl, limits)?;
    if records.is_empty() {
        return Ok(());
    }
    let schema = match &options.schema {
        Some(schema) => schema.clone(),
        None => infer_schema(&records)?,
    };
    let batch_size = options.batch_size.max(1);
    for chunk in records.chunks(batch_size) {
        emit(build_batch(chunk, &schema)?)?;
    }
    Ok(())
}

struct CappedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    overflow: Rc<Cell<usize>>,
}

impl CappedBuffer {
    fn new(limit: usize) -> (Self, Rc<Cell<usize>>) {
        let overflow = Rc::new(Cell::new(0));
        (
            Self {
                bytes: Vec::with_capacity(limit.min(64 * 1024)),
                limit,
                overflow: overflow.clone(),
            },
            overflow,
        )
    }
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let observed = self.bytes.len().saturating_add(bytes.len());
        if observed > self.limit {
            self.overflow.set(self.overflow.get().max(observed));
            return Err(std::io::Error::other("json Arrow IPC byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn ipc_error(
    stage: &str,
    error: impl std::fmt::Display,
    overflow: &Cell<usize>,
    limit: usize,
) -> JsonError {
    let observed = overflow.get();
    if observed > limit {
        JsonError::ResourceLimit {
            resource: JsonResource::IpcBytes,
            limit,
            observed,
        }
    } else {
        JsonError::Structure(format!("arrow ipc {stage}: {error}"))
    }
}

/// Encode a JSONL document as an in-memory Arrow IPC stream.
///
/// Empty input returns an empty byte vector. Uses the same schema
/// resolution and batching rules as [`parse_jsonl_to_arrow_batches`].
pub fn parse_jsonl_to_arrow_ipc(jsonl: &str, options: &ArrowOptions) -> Result<Vec<u8>> {
    parse_jsonl_to_arrow_ipc_with_limits(jsonl, options, &JsonLimits::default())
}

/// Encode bounded JSONL as an in-memory Arrow IPC stream.
pub fn parse_jsonl_to_arrow_ipc_with_limits(
    jsonl: &str,
    options: &ArrowOptions,
    limits: &JsonLimits,
) -> Result<Vec<u8>> {
    let records: Vec<Value> = crate::jsonl::parse_many_with_limits(jsonl, limits)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let schema = match &options.schema {
        Some(schema) => schema.clone(),
        None => infer_schema(&records)?,
    };
    let (buffer, overflow) = CappedBuffer::new(limits.max_ipc_bytes);
    let mut writer = StreamWriter::try_new(buffer, &schema)
        .map_err(|error| ipc_error("writer", error, overflow.as_ref(), limits.max_ipc_bytes))?;
    let batch_size = options.batch_size.max(1);
    for chunk in records.chunks(batch_size) {
        writer
            .write(&build_batch(chunk, &schema)?)
            .map_err(|error| ipc_error("write", error, overflow.as_ref(), limits.max_ipc_bytes))?;
    }
    writer
        .finish()
        .map_err(|error| ipc_error("finish", error, overflow.as_ref(), limits.max_ipc_bytes))?;
    let buffer = writer
        .into_inner()
        .map_err(|error| ipc_error("finalize", error, overflow.as_ref(), limits.max_ipc_bytes))?;
    Ok(buffer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_ipc::reader::StreamReader;
    use std::io::Cursor;

    const DOC: &str = "{\"id\":1,\"name\":\"ana\",\"active\":true,\"meta\":{\"k\":1}}\n\
                       {\"id\":2,\"name\":\"bia\",\"active\":false,\"score\":1.5}\n\
                       {\"id\":3,\"active\":true}\n";

    #[test]
    fn infers_union_schema_with_unified_types() {
        let records: Vec<Value> = crate::jsonl::parse_many(DOC).unwrap();
        let schema = infer_schema(&records).unwrap();
        let types = schema
            .fields()
            .iter()
            .map(|f| (f.name().as_str(), f.data_type().clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                ("id", DataType::Int64),
                ("name", DataType::Utf8),
                ("active", DataType::Boolean),
                ("meta", DataType::Utf8),
                ("score", DataType::Float64),
            ]
        );
    }

    #[test]
    fn batch_rows_match_record_count_and_nulls() {
        let records: Vec<Value> = crate::jsonl::parse_many(DOC).unwrap();
        let schema = infer_schema(&records).unwrap();
        let batch = build_batch(&records, &schema).unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 5);
        // `name` has a missing value on row 3 → null.
        let names = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        assert!(names.is_null(2));
    }

    #[test]
    fn streams_batches_at_requested_size() {
        let mut count = 0usize;
        let mut total_rows = 0usize;
        let options = ArrowOptions {
            batch_size: 2,
            schema: None,
        };
        parse_jsonl_to_arrow_batches(DOC, &options, |batch| {
            count += 1;
            total_rows += batch.num_rows();
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(total_rows, 3);
    }

    #[test]
    fn explicit_schema_is_respected() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let options = ArrowOptions {
            batch_size: 1,
            schema: Some(schema.clone()),
        };
        let got = jsonl_schema(DOC, &options).unwrap();
        assert_eq!(got, schema);
    }

    #[test]
    fn rejects_non_object_lines() {
        assert!(infer_schema(&[Value::from(1)]).is_err());
    }

    #[test]
    fn explicit_schema_parses_rfc3339_timestamps_to_utc_micros() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "occurred_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        )]));
        let doc = "{\"occurred_at\":\"2026-08-18T13:59:00Z\"}\n\
                   {\"occurred_at\":\"2026-08-18T15:59:00+02:00\"}\n";
        let batch = build_batch(&crate::jsonl::parse_many(doc).unwrap(), &schema).unwrap();
        let ts = batch
            .column_by_name("occurred_at")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .unwrap();
        // 2026-08-18T13:59:00Z = 1_787_061_540s; both rows are the same instant.
        assert_eq!(ts.value(0), 1_787_061_540_000_000i64);
        assert_eq!(ts.value(1), 1_787_061_540_000_000i64);
        assert_eq!(
            ts.data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }

    #[test]
    fn explicit_schema_rejects_non_timestamp_values_fail_loud() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "occurred_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        )]));
        let records = crate::jsonl::parse_many("{\"occurred_at\":123}\n").unwrap();
        let err = build_batch(&records, &schema).unwrap_err();
        assert!(err.to_string().contains("expected an RFC-3339 string"));
    }

    #[test]
    fn arrow_ipc_is_returned_in_memory_and_decodes() {
        let payload = parse_jsonl_to_arrow_ipc(DOC, &ArrowOptions::default())
            .expect("valid JSONL must encode to in-memory IPC");
        let rows = StreamReader::try_new(Cursor::new(payload), None)
            .expect("IPC stream must open")
            .map(|batch| batch.expect("IPC batch must decode").num_rows())
            .sum::<usize>();
        assert_eq!(rows, 3);
    }

    #[test]
    fn arrow_ipc_output_is_bounded_before_buffer_growth() {
        let limits = crate::JsonLimits {
            max_ipc_bytes: 16,
            ..Default::default()
        };
        let error = parse_jsonl_to_arrow_ipc_with_limits(DOC, &ArrowOptions::default(), &limits)
            .expect_err("an undersized IPC budget must fail closed");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: crate::JsonResource::IpcBytes,
                limit: 16,
                ..
            }
        ));
    }
}
