//! A malformed package must be rejected with an error: never a panic, a stack
//! overflow, or an allocation out of proportion to the file.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use ooxml_archive::{
    normalize_part, parse_relationships, parse_root_rels, part_dir, relationship_matches,
    relationship_target, rels_path_for_part, resolve_part, source_part_from_rels, OoxmlArchive,
    REL_OFFICE_DOCUMENT,
};
use std::io::{Cursor, Read, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

/// Inflate ceiling used by the smoke runs: far above the seed, far below the
/// allocation cap, so a part that ignores it is caught by the allocator.
const MAX_INFLATE: usize = 4 * 1024 * 1024;

const ROOT_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="/docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/extendedProperties" Target="docProps\app.xml" TargetMode="Internal"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/?a=1&amp;b=2" TargetMode="External"/>
</Relationships>"#;

/// A package with stored and deflated parts, a directory entry, a part in a
/// nested folder, and a part large enough to span several deflate blocks.
fn seed_package(method: CompressionMethod) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
    let mut body = Vec::new();
    for index in 0..60 {
        body.extend_from_slice(format!("<w:p><w:r><w:t>line {index}</w:t></w:r></w:p>").as_bytes());
    }
    zip.start_file("[Content_Types].xml", options).unwrap();
    zip.write_all(b"<Types/>").unwrap();
    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(ROOT_RELS).unwrap();
    zip.add_directory("word/", SimpleFileOptions::default())
        .unwrap();
    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(&body).unwrap();
    zip.start_file(
        "word/_rels/document.xml.rels",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(ROOT_RELS).unwrap();
    zip.start_file(
        "word/media/image1.png",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )
    .unwrap();
    zip.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
    zip.set_comment("smoke").unwrap();
    zip.finish().unwrap().into_inner()
}

/// Opens `bytes` and reads everything the archive exposes.
fn read_everything(bytes: &[u8]) {
    let Ok(archive) = OoxmlArchive::from_bytes(bytes.to_vec()) else {
        return;
    };
    let names: Vec<String> = archive.part_names().map(str::to_string).collect();
    for name in &names {
        assert!(archive.has_part(name));
        if let Ok(part) = archive.read_part(name, MAX_INFLATE) {
            assert!(part.as_slice().len() <= MAX_INFLATE);
            let _ = parse_relationships(&part);
        }
        if let Ok(mut reader) = archive.open_part_reader(name, MAX_INFLATE, true) {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        }
        exercise_paths(name);
    }
    let _ = archive.office_document_path(MAX_INFLATE, "word/document.xml");
    let _ = archive.read_part("no/such/part.xml", MAX_INFLATE);
}

/// Runs every path helper over `text`, as both a part name and a target.
fn exercise_paths(text: &str) {
    let _ = normalize_part(text);
    let dir = part_dir(text);
    let _ = resolve_part(&dir, text);
    let _ = resolve_part(text, "../x.xml");
    let rels = rels_path_for_part(text);
    let _ = source_part_from_rels(&rels);
    let _ = source_part_from_rels(text);
    let _ = relationship_matches(text, REL_OFFICE_DOCUMENT);
    let _ = relationship_matches(REL_OFFICE_DOCUMENT, text);
}

fn read_relationships(xml: &[u8]) {
    let _ = parse_root_rels(xml);
    if let Ok(rels) = parse_relationships(xml) {
        let _ = relationship_target(&rels, REL_OFFICE_DOCUMENT);
        for entry in &rels.entries {
            exercise_paths(&entry.target);
            exercise_paths(&entry.rel_type);
        }
    }
    exercise_paths(&String::from_utf8_lossy(xml));
}

#[test]
fn the_seed_package_reads_back() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        let archive = OoxmlArchive::from_bytes(seed_package(method)).expect("seed opens");
        assert_eq!(archive.part_names().count(), 5);
        assert_eq!(
            archive
                .office_document_path(MAX_INFLATE, "missing.xml")
                .expect("root relationships resolve"),
            "word/document.xml"
        );
        let document = archive
            .read_part("word/document.xml", MAX_INFLATE)
            .expect("document reads");
        assert!(document.ends_with(b"<w:p><w:r><w:t>line 59</w:t></w:r></w:p>"));
    }
}

#[test]
fn mutated_containers_never_panic() {
    let seeds = [
        seed_package(CompressionMethod::Stored),
        seed_package(CompressionMethod::Deflated),
    ];
    smoke::assert_no_panics("ooxml-archive container", &seeds, 12_000, read_everything);
}

#[test]
fn mutated_relationships_never_panic() {
    smoke::assert_no_panics(
        "ooxml-archive relationships",
        &[ROOT_RELS.to_vec()],
        15_000,
        read_relationships,
    );
}

#[test]
fn deep_nesting_is_bounded_on_a_small_stack() {
    const DEPTH: usize = 100_000;
    let nested_rels = smoke::nested(
        b"<Relationships><Relationship Id=\"a\" Type=\"t\" Target=\"x\">",
        b"",
        b"</Relationship></Relationships>",
        DEPTH,
    );
    let climbing = "../".repeat(DEPTH) + "word/document.xml";
    let descending = "a/".repeat(DEPTH) + "b.xml";
    smoke::run_on_small_stack(move || {
        read_relationships(&nested_rels);
        assert_eq!(resolve_part("word/_rels", &climbing), "word/document.xml");
        assert_eq!(part_dir(&descending).len(), 2 * DEPTH - 1);
        exercise_paths(&climbing);
        exercise_paths(&descending);
    });
}

/// A part that inflates far past the caller's limit is cut off at the limit,
/// whatever sizes its ZIP headers declare.
#[test]
fn a_part_that_inflates_past_the_limit_is_cut_off_at_the_limit() {
    const LIMIT: usize = 64 * 1024;
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("word/document.xml", options).unwrap();
    let chunk = vec![0u8; 1024 * 1024];
    for _ in 0..48 {
        zip.write_all(&chunk).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    assert!(bytes.len() < 256 * 1024, "the seed is a small file");

    let archive = OoxmlArchive::from_bytes(bytes).expect("package opens");
    let error = archive
        .read_part("word/document.xml", LIMIT)
        .expect_err("48 MiB does not fit a 64 KiB limit");
    assert!(error.to_string().contains("max_inflate"), "{error:#}");
    assert!(archive
        .open_part_reader("word/document.xml", LIMIT, true)
        .is_err());
}

/// quick-xml 0.36 checks each attribute name against all earlier ones
/// (RUSTSEC-2026-0194), so this is quadratic until the dependency moves. The
/// time is printed, not asserted: the test only pins that it finishes.
#[test]
fn a_start_tag_with_twenty_thousand_attributes_is_parsed() {
    let mut xml = b"<Relationships><Relationship".to_vec();
    for index in 0..20_000 {
        xml.extend_from_slice(format!(" a{index}=\"v\"").as_bytes());
    }
    xml.extend_from_slice(b" Id=\"rId1\" Type=\"t\" Target=\"kept.xml\"/></Relationships>");

    let started = std::time::Instant::now();
    let rels = parse_relationships(&xml).expect("relationships parse");
    eprintln!(
        "ooxml-archive: start tag with 20,000 attributes parsed in {:?}",
        started.elapsed()
    );
    assert_eq!(rels.by_id.get("rId1").map(String::as_str), Some("kept.xml"));
}
