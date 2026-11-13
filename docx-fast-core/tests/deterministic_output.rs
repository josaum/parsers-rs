//! The same input must give the same bytes every time.
//!
//! Metadata and settings are held in `HashMap`s, and every `HashMap` gets a
//! different seed, so two parses in one process iterate the same keys in
//! different orders (the first test shows that this holds here). Anything
//! written from such a map, JSON, NDJSON or Arrow rows, must not follow that
//! order.

use std::collections::HashMap;
use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const RUNS: usize = 20;
const KEYS: usize = 40;

fn package(parts: &[(&str, String)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in parts {
        zip.start_file(*name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn simple_props(root: &str) -> String {
    let mut xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><{root}>"#);
    for index in 0..KEYS {
        xml.push_str(&format!("<prop{index}>value {index}</prop{index}>"));
    }
    xml.push_str(&format!("</{root}>"));
    xml
}

fn custom_props() -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><Properties xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">"#,
    );
    for index in 0..KEYS {
        xml.push_str(&format!(
            r#"<property pid="{}" name="custom{index}"><vt:lpwstr>value {index}</vt:lpwstr></property>"#,
            index + 2
        ));
    }
    xml.push_str("</Properties>");
    xml
}

fn assert_all_equal(outputs: &[Vec<u8>], what: &str) {
    assert_eq!(outputs.len(), RUNS);
    assert!(!outputs[0].is_empty(), "{what} is empty");
    for (run, output) in outputs.iter().enumerate() {
        assert!(
            output == &outputs[0],
            "{what} of run {run} differs from run 0"
        );
    }
}

/// The premise of this file: maps with the same keys iterate differently
/// within one process, so repeating a parse does exercise the ordering.
#[test]
fn hash_maps_in_one_process_iterate_in_different_orders() {
    let orders: Vec<Vec<usize>> = (0..RUNS)
        .map(|_| {
            let map: HashMap<usize, ()> = (0..KEYS).map(|key| (key, ())).collect();
            map.into_keys().collect()
        })
        .collect();
    assert!(orders.iter().any(|order| order != &orders[0]));
}

use docx_fast_core::{parse_docx_bytes, parse_docx_to_ndjson, ParseOptions};

fn document() -> Vec<u8> {
    let mut settings = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    );
    for index in 0..KEYS {
        settings.push_str(&format!(r#"<w:setting{index} w:val="{index}"/>"#));
    }
    settings.push_str("</w:settings>");
    package(&[
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_string(),
        ),
        (
            "word/document.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#.to_string(),
        ),
        ("word/settings.xml", settings),
        ("docProps/core.xml", simple_props("coreProperties")),
        ("docProps/app.xml", simple_props("Properties")),
        ("docProps/custom.xml", custom_props()),
    ])
}

#[test]
fn json_is_the_same_on_every_parse() {
    let bytes = document();
    let parsed = parse_docx_bytes(bytes.clone(), ParseOptions::default()).unwrap();
    assert_eq!(parsed.metadata.core.len(), KEYS);
    assert_eq!(parsed.metadata.app.len(), KEYS);
    assert_eq!(parsed.metadata.custom.len(), KEYS);
    assert_eq!(parsed.settings.len(), KEYS);

    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|_| {
            let document = parse_docx_bytes(bytes.clone(), ParseOptions::default()).unwrap();
            serde_json::to_vec(&document).unwrap()
        })
        .collect();
    assert_all_equal(&outputs, "JSON");
}

#[test]
fn ndjson_is_the_same_on_every_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.docx");
    std::fs::write(&path, document()).unwrap();
    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|run| {
            let out = dir.path().join(format!("out{run}.ndjson"));
            parse_docx_to_ndjson(&path, &out, ParseOptions::default()).unwrap();
            std::fs::read(out).unwrap()
        })
        .collect();
    assert_all_equal(&outputs, "NDJSON");
}

#[cfg(feature = "arrow")]
#[test]
fn arrow_ipc_is_the_same_on_every_parse() {
    use docx_fast_core::arrow::{parse_docx_to_arrow_dataset, ArrowOptions};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.docx");
    std::fs::write(&path, document()).unwrap();
    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|run| {
            let out_dir = dir.path().join(format!("out{run}"));
            let mut written = parse_docx_to_arrow_dataset(
                &path,
                &out_dir,
                ParseOptions::default(),
                ArrowOptions::default(),
            )
            .unwrap();
            written.sort();
            let mut all = Vec::new();
            for file in written {
                all.extend_from_slice(file.file_name().unwrap().as_encoded_bytes());
                all.extend(std::fs::read(file).unwrap());
            }
            all
        })
        .collect();
    assert_all_equal(&outputs, "Arrow IPC");
}
