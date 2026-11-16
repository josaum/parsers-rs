use ooxml_archive::OoxmlArchive;
use std::io::{Cursor, Write};
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

fn package(method: CompressionMethod) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file("doc.xml", FileOptions::default().compression_method(method))
        .unwrap();
    writer.write_all(b"<document>hello</document>").unwrap();
    writer.finish().unwrap().into_inner()
}

fn central_directory(bytes: &[u8]) -> usize {
    bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .unwrap()
}

#[test]
fn rejects_invalid_checksum_including_zero_in_both_read_apis() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        for crc in [0u32, 42u32] {
            let mut bytes = package(method);
            let offset = central_directory(&bytes) + 16;
            bytes[offset..offset + 4].copy_from_slice(&crc.to_le_bytes());
            let archive = OoxmlArchive::from_bytes(bytes).unwrap();
            assert!(archive.read_part("doc.xml", 1024).is_err());
            assert!(archive.open_part_reader("doc.xml", 1024, true).is_err());
        }
    }
}

#[test]
fn rejects_incorrect_uncompressed_size_in_both_read_apis() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        let mut bytes = package(method);
        let offset = central_directory(&bytes) + 24;
        bytes[offset..offset + 4].copy_from_slice(&1u32.to_le_bytes());
        let archive = OoxmlArchive::from_bytes(bytes).unwrap();
        assert!(archive.read_part("doc.xml", 1024).is_err());
        assert!(archive.open_part_reader("doc.xml", 1024, false).is_err());
    }
}

#[test]
fn rejects_duplicate_normalized_part_names() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for name in ["doc.xml", "./doc.xml"] {
        writer.start_file(name, FileOptions::default()).unwrap();
        writer.write_all(b"<doc/>").unwrap();
    }
    assert!(OoxmlArchive::from_bytes(writer.finish().unwrap().into_inner()).is_err());
}

#[test]
fn accepts_empty_parts_with_a_legitimate_zero_crc_and_zero_budget() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "empty.xml",
                FileOptions::default().compression_method(method),
            )
            .unwrap();
        let archive = OoxmlArchive::from_bytes(writer.finish().unwrap().into_inner()).unwrap();
        assert!(archive.read_part("empty.xml", 0).unwrap().is_empty());
        assert!(archive.open_part_reader("empty.xml", 0, true).is_ok());
    }
}
