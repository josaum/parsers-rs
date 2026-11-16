//! An archive whose central directory lists fewer entries than it declares
//! is refused: opening it would hide parts without saying so.

use ooxml_archive::{OoxmlArchive, DEFAULT_MAX_INFLATE};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Size of an end-of-central-directory record without a comment.
const EOCD_SIZE: usize = 22;
/// Signature of a central directory file header.
const CENTRAL_HEADER: &[u8] = b"PK\x01\x02";
/// A digital-signature record with no data: signature, then a zero length.
const DIGITAL_SIGNATURE: &[u8] = b"PK\x05\x05\x00\x00";

fn package(names: &[&str]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for name in names {
        zip.start_file(*name, options).unwrap();
        zip.write_all(b"<ok/>").unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

/// Plants a digital-signature record before central directory header
/// `before` (0-based) and grows the declared directory size to match.
fn with_signature_record(mut bytes: Vec<u8>, before: usize) -> Vec<u8> {
    let eocd = bytes.len() - EOCD_SIZE;
    assert_eq!(&bytes[eocd..eocd + 4], b"PK\x05\x06");
    let directory_size = le32(&bytes, eocd + 12);
    let directory = le32(&bytes, eocd + 16);
    let headers: Vec<usize> = (directory..eocd)
        .filter(|at| bytes[*at..].starts_with(CENTRAL_HEADER))
        .collect();
    let grown = u32::try_from(directory_size + DIGITAL_SIGNATURE.len()).unwrap();
    bytes[eocd + 12..eocd + 16].copy_from_slice(&grown.to_le_bytes());
    let at = headers[before];
    bytes.splice(at..at, DIGITAL_SIGNATURE.iter().copied());
    bytes
}

#[test]
fn a_signature_record_inside_the_directory_does_not_hide_entries() {
    let names = ["word/document.xml", "word/styles.xml"];
    let plain = package(&names);
    let archive = OoxmlArchive::from_bytes(plain.clone()).expect("the plain archive opens");
    assert!(names.iter().all(|name| archive.has_part(name)));

    // Between the two entries, and before the first.
    for before in [1, 0] {
        let err = OoxmlArchive::from_bytes(with_signature_record(plain.clone(), before))
            .expect_err("an archive with hidden entries must not open");
        assert!(
            format!("{err:#}").contains("declares 2"),
            "record before entry {before}: {err:#}"
        );
    }
}

#[test]
fn ordinary_archives_still_open() {
    // Directories and a repeated name are entries like any other.
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.add_directory("word/", options).unwrap();
    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(b"<ok/>").unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let archive = OoxmlArchive::from_bytes(bytes).expect("an archive with a directory opens");
    let part = archive
        .read_part("word/document.xml", DEFAULT_MAX_INFLATE)
        .unwrap();
    assert_eq!(part.as_slice(), b"<ok/>");

    let empty = ZipWriter::new(Cursor::new(Vec::new()))
        .finish()
        .unwrap()
        .into_inner();
    OoxmlArchive::from_bytes(empty).expect("an empty archive opens");
}

#[test]
fn an_archive_at_the_classic_entry_limit_opens() {
    // 65535 is both a real count and the value that means "see zip64".
    for count in [65_534_usize, 65_535, 65_536] {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for index in 0..count {
            zip.start_file(format!("p/{index}"), options).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let archive = OoxmlArchive::from_bytes(bytes)
            .unwrap_or_else(|err| panic!("{count} entries: {err:#}"));
        assert_eq!(archive.part_names().count(), count);
    }
}

#[test]
fn part_names_are_sorted_byte_by_byte_whatever_the_archive_order() {
    let archive =
        OoxmlArchive::from_bytes(package(&["z.xml", "a/b.xml", "A.xml", "m.xml", "a.xml"]))
            .unwrap();
    let names: Vec<&str> = archive.part_names().collect();
    assert_eq!(names, ["A.xml", "a.xml", "a/b.xml", "m.xml", "z.xml"]);
    // The same on every call, and nothing is lost to the ordering.
    assert_eq!(archive.part_names().collect::<Vec<_>>(), names);
    assert!(names.iter().all(|name| archive.has_part(name)));
}
