//! Apache Arrow materialization for deterministic web parse results.

use crate::{block_kind_name, WebBlock, WebDocument, WebParseError};
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
    blocks_to_record_batch(&document.source_sha256, &document.blocks)
}

fn blocks_to_record_batch(
    source_sha256: &str,
    blocks: &[WebBlock],
) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let mut source = StringBuilder::new();
    let mut ordinal = UInt32Builder::new();
    let mut kind = StringBuilder::new();
    let mut dom_path = StringBuilder::new();
    let mut text = StringBuilder::new();
    let mut heading_level = UInt8Builder::new();

    for block in blocks {
        source.append_value(source_sha256);
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

/// Write document-order web blocks as an Arrow IPC stream in batches of 1,024 rows.
pub fn write_web_blocks_ipc<W: Write>(
    document: &WebDocument,
    writer: &mut W,
) -> Result<(), WebParseError> {
    write_web_blocks_ipc_batched(document, writer, 1024)
}

/// Write blocks in batches with at most `batch_size` rows and flush the writer.
///
/// A zero batch size is rejected before writing. An empty document emits one
/// empty batch. On an I/O error the destination may contain an incomplete stream.
pub fn write_web_blocks_ipc_batched<W: Write>(
    document: &WebDocument,
    writer: &mut W,
    batch_size: usize,
) -> Result<(), WebParseError> {
    if batch_size == 0 {
        return Err(WebParseError::Arrow(
            "batch size must be greater than zero".into(),
        ));
    }
    let schema = web_blocks_schema();
    let mut stream = StreamWriter::try_new(&mut *writer, &schema)
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    if document.blocks.is_empty() {
        let batch = blocks_to_record_batch(&document.source_sha256, &[])
            .map_err(|error| WebParseError::Arrow(error.to_string()))?;
        stream
            .write(&batch)
            .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    } else {
        for blocks in document.blocks.chunks(batch_size) {
            let batch = blocks_to_record_batch(&document.source_sha256, blocks)
                .map_err(|error| WebParseError::Arrow(error.to_string()))?;
            stream
                .write(&batch)
                .map_err(|error| WebParseError::Arrow(error.to_string()))?;
        }
    }
    stream
        .finish()
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    drop(stream);
    writer
        .flush()
        .map_err(|error| WebParseError::Arrow(error.to_string()))?;
    Ok(())
}
