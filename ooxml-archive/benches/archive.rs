//! Opening an archive and reading every part of it.
//!
//! The input is a 2,000-entry package built in memory, once stored and once
//! deflated. Each part is about 4 KiB of XML-like text.

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::{Cursor, Write};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use ooxml_archive::OoxmlArchive;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const ENTRIES: usize = 2_000;
const MAX_INFLATE: usize = 64 * 1024 * 1024;

fn part_name(index: usize) -> String {
    format!("xl/worksheets/part{index:04}.xml")
}

fn part_body(index: usize) -> String {
    let mut body = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><part>"#);
    let mut state = index as u32 * 2_654_435_761u32.wrapping_add(1);
    while body.len() < 4_096 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        write!(
            body,
            r#"<item id="{}">value {}</item>"#,
            state >> 20,
            state % 977
        )
        .unwrap();
    }
    body.push_str("</part>");
    body
}

/// The generated package and the total size of its parts before compression.
fn package(method: CompressionMethod) -> (Vec<u8>, usize) {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
    let mut inflated = 0;
    for index in 0..ENTRIES {
        let body = part_body(index);
        zip.start_file(part_name(index), options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        inflated += body.len();
    }
    (zip.finish().unwrap().into_inner(), inflated)
}

fn read_all(bytes: Vec<u8>, names: &[String]) -> usize {
    let archive = OoxmlArchive::from_bytes(bytes).unwrap();
    let mut total = 0;
    for name in names {
        total += archive.read_part(name, MAX_INFLATE).unwrap().len();
    }
    total
}

fn bench(c: &mut Criterion) {
    let names: Vec<String> = (0..ENTRIES).map(part_name).collect();
    for (label, method) in [
        ("stored", CompressionMethod::Stored),
        ("deflated", CompressionMethod::Deflated),
    ] {
        let (bytes, inflated) = package(method);
        assert_eq!(read_all(bytes.clone(), &names), inflated);
        assert_eq!(
            OoxmlArchive::from_bytes(bytes.clone())
                .unwrap()
                .part_names()
                .count(),
            ENTRIES
        );
        eprintln!(
            "ooxml-archive {label} input: {ENTRIES} parts, {} bytes zipped, {inflated} bytes of parts",
            bytes.len()
        );

        let mut group = c.benchmark_group(format!("archive_2000_entries_{label}"));
        group
            .throughput(Throughput::Bytes(inflated as u64))
            .sample_size(30)
            .measurement_time(Duration::from_secs(5));
        group.bench_function("open", |b| {
            b.iter_batched(
                || bytes.clone(),
                |bytes| black_box(OoxmlArchive::from_bytes(bytes).unwrap()),
                BatchSize::LargeInput,
            );
        });
        group.bench_function("open_and_read_all", |b| {
            b.iter_batched(
                || bytes.clone(),
                |bytes| black_box(read_all(bytes, &names)),
                BatchSize::LargeInput,
            );
        });
        group.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
