//! Rebuilds a seed OOXML package with one part replaced by fuzz input.
//!
//! A fuzzer that mutates a whole `.docx`, `.xlsx` or `.pptx` spends nearly all
//! of its time breaking the ZIP container: one changed byte fails a checksum
//! and the XML readers are never reached. The targets that include this file
//! keep a valid package and swap in the input as one part, uncompressed, so
//! coverage feedback comes from the XML reader of that part.
//!
//! Included by path from the `fuzz/` crates, like `mutation_smoke.rs` is from
//! the tests, so it adds no workspace member.

use std::io::{Cursor, Read, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Every part of `seed`, in order, as `(name, bytes)`.
///
/// # Panics
///
/// Panics if `seed` is not a readable ZIP file: it is a checked-in fixture.
pub fn parts(seed: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut archive = ZipArchive::new(Cursor::new(seed)).expect("seed package opens");
    (0..archive.len())
        .map(|index| {
            let mut entry = archive.by_index(index).expect("seed entry opens");
            let mut body = Vec::new();
            entry.read_to_end(&mut body).expect("seed entry reads");
            let name = entry.name().expect("seed entry is named in UTF-8");
            (name.into_owned(), body)
        })
        .collect()
}

/// A stored (uncompressed) package of `parts`, with `body` in place of the
/// part called `name`.
///
/// # Panics
///
/// Panics if no part is called `name`: the target would then parse the same
/// unchanged package on every run and report coverage for none of its input.
/// That is a mistake in the harness, never a property of the input. Also
/// panics if the in-memory ZIP writer fails, which it does not for names
/// taken from a valid package.
pub fn with_part(parts: &[(String, Vec<u8>)], name: &str, body: &[u8]) -> Vec<u8> {
    assert!(
        parts.iter().any(|(part, _)| part == name),
        "the seed package has no part called {name}"
    );
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (part, original) in parts {
        zip.start_file(part.as_str(), options)
            .expect("start zip entry");
        zip.write_all(if part == name { body } else { original })
            .expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}
