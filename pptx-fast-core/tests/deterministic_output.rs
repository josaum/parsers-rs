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

use pptx_fast_core::{parse_pptx_bytes, parse_pptx_to_ndjson, ParseOptions};

fn deck() -> Vec<u8> {
    package(&[
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#.to_string(),
        ),
        (
            "ppt/presentation.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#.to_string(),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#.to_string(),
        ),
        (
            "ppt/slides/slide1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Body</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#.to_string(),
        ),
        ("docProps/core.xml", simple_props("coreProperties")),
        ("docProps/app.xml", simple_props("Properties")),
        ("docProps/custom.xml", custom_props()),
    ])
}

#[test]
fn json_is_the_same_on_every_parse() {
    let bytes = deck();
    let parsed = parse_pptx_bytes(bytes.clone(), ParseOptions::default()).unwrap();
    assert_eq!(parsed.slides.len(), 1);
    assert_eq!(parsed.metadata.core.len(), KEYS);
    assert_eq!(parsed.metadata.app.len(), KEYS);
    assert_eq!(parsed.metadata.custom.len(), KEYS);

    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|_| {
            let document = parse_pptx_bytes(bytes.clone(), ParseOptions::default()).unwrap();
            serde_json::to_vec(&document).unwrap()
        })
        .collect();
    assert_all_equal(&outputs, "JSON");
}

#[test]
fn ndjson_is_the_same_on_every_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.pptx");
    std::fs::write(&path, deck()).unwrap();
    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|run| {
            let out = dir.path().join(format!("out{run}.ndjson"));
            parse_pptx_to_ndjson(&path, &out, ParseOptions::default()).unwrap();
            std::fs::read(out).unwrap()
        })
        .collect();
    assert_all_equal(&outputs, "NDJSON");
}

#[cfg(feature = "arrow")]
#[test]
fn arrow_ipc_is_the_same_on_every_parse() {
    use pptx_fast_core::arrow::{list_tables, parse_pptx_to_arrow_ipc_bytes, ArrowOptions};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.pptx");
    std::fs::write(&path, deck()).unwrap();
    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|_| {
            let mut all = Vec::new();
            for table in list_tables() {
                all.extend(
                    parse_pptx_to_arrow_ipc_bytes(
                        &path,
                        ParseOptions::default(),
                        ArrowOptions::default(),
                        table,
                    )
                    .unwrap(),
                );
            }
            all
        })
        .collect();
    assert_all_equal(&outputs, "Arrow IPC");
}
