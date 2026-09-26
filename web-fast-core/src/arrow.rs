//! Apache Arrow materialization for deterministic web parse results.

use crate::{block_kind_name, WebDocument, WebParseError};
use arrow_array::{
    builder::{StringBuilder, UInt32Builder, UInt8Builder},
    RecordBatch,
};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema};
use std::io::Write;
use std::sync::Arc;

/// Stable Arrow schema for document-order web blocks.
pub fn web_blocks_schema() -> Schema {
    Schema::new(vec![
        Field::new("source_sha256", DataType::Utf8, false),
        Field::new("ordinal", DataType::UInt32, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("dom_path", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("heading_level", DataType::UInt8, true),
    ])
}

/// Materialize a parsed document's blocks as one Arrow [`RecordBatch`].
pub fn web_blocks_to_record_batch(
    document: &WebDocument,
) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let mut source = StringBuilder::new();
    let mut ordinal = UInt32Builder::new();
    let mut kind = StringBuilder::new();
    let mut dom_path = StringBuilder::new();
    let mut text = StringBuilder::new();
    let mut heading_level = UInt8Builder::new();

    for block in &document.blocks {
        source.append_value(&document.source_sha256);
        ordinal.append_value(block.ordinal);
        kind.append_value(block_kind_name(block.kind));
        dom_path.append_value(&block.dom_path);
        text.append_value(&block.text);
        heading_level.append_option(block.heading_level);
    }

    RecordBatch::try_new(
        Arc::new(web_blocks_schema()),
        vec![
            Arc::new(source.finish()),
            Arc::new(ordinal.finish()),
            Arc::new(kind.finish()),
            Arc::new(dom_path.finish()),
            Arc::new(text.finish()),
            Arc::new(heading_level.finish()),
        ],
    )
}

/// Write document-order web blocks as an Arrow IPC stream.
pub fn write_web_blocks_ipc<W: Write>(
    document: &WebDocument,
    writer: &mut W,
) -> Result<(), WebParseError> {
    let batch = web_blocks_to_record_batch(document)
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    let mut stream = StreamWriter::try_new(writer, &batch.schema())
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    stream
        .write(&batch)
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    stream
        .finish()
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    Ok(())
}
