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

#[test]
fn batches_bound_materialization_and_preserve_global_ordinals() {
    use arrow_array::UInt32Array;
    use web_fast_core::arrow::write_web_blocks_ipc_batched;
    let document = parse_html(&"<p>x</p>".repeat(5), None, ParseOptions::default()).unwrap();
    let mut bytes = Vec::new();
    write_web_blocks_ipc_batched(&document, &mut bytes, 2).unwrap();
    let batches = StreamReader::try_new(Cursor::new(bytes), None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.num_rows())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    let last = batches[2]
        .column(1)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap();
    assert_eq!(last.value(0), 4);
    let mut bytes = Vec::new();
    assert!(write_web_blocks_ipc_batched(&document, &mut bytes, 0).is_err());
    assert!(bytes.is_empty());
}

#[test]
fn empty_stream_and_flush_errors_are_handled() {
    let document = parse_html("", None, ParseOptions::default()).unwrap();
    let mut bytes = Vec::new();
    write_web_blocks_ipc(&document, &mut bytes).unwrap();
    let batches = StreamReader::try_new(Cursor::new(bytes), None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 0);
    struct FlushFailure;
    impl std::io::Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }
    assert!(write_web_blocks_ipc(&document, &mut FlushFailure).is_err());
}
