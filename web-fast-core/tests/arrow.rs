#![cfg(feature = "arrow")]

use arrow_ipc::reader::StreamReader;
use std::io::Cursor;
use web_fast_core::arrow::{web_blocks_to_record_batch, write_web_blocks_ipc};
use web_fast_core::{parse_html, ParseOptions};

#[test]
fn arrow_batch_and_ipc_preserve_document_order() {
    let document = parse_html(
        "<html><body><h1>Title</h1><p>Body</p></body></html>",
        None,
        ParseOptions::default(),
    )
    .expect("parse HTML");
    let batch = web_blocks_to_record_batch(&document).expect("Arrow batch");
    assert_eq!(batch.num_rows(), 2);
    assert_eq!(batch.schema().field(1).name(), "ordinal");

    let mut bytes = Vec::new();
    write_web_blocks_ipc(&document, &mut bytes).expect("Arrow IPC");
    let decoded = StreamReader::try_new(Cursor::new(bytes), None)
        .expect("IPC reader")
        .collect::<Result<Vec<_>, _>>()
        .expect("IPC batches");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].num_rows(), 2);
}
