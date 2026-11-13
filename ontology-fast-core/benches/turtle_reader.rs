//! Throughput of every public reader entry point over a 50 MB Turtle document.
//!
//! Run with `cargo bench -p ontology-fast-core`. Criterion reports MB per
//! second by default; set `ONTOLOGY_BENCH_THROUGHPUT=triples` to have it report
//! triples per second instead. The document's triple count is printed once so
//! either rate can be derived from the other.

mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use ontology_fast_core::{declared_prefixes, parse_turtle, parse_turtle_terms, OntologyLimits};

fn reader_entry_points(criterion: &mut Criterion) {
    let document = support::synthetic_turtle(support::BENCH_DOCUMENT_BYTES);
    let limits = OntologyLimits::default();
    let triples = parse_turtle_terms(&document, limits)
        .expect("the benchmark document parses within the default limits")
        .len();
    eprintln!(
        "turtle_reader: document {} bytes, {triples} distinct triples",
        document.len()
    );

    let by_triples = std::env::var("ONTOLOGY_BENCH_THROUGHPUT").is_ok_and(|v| v == "triples");
    let mut group = criterion.benchmark_group("turtle_50mb");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(20));
    group.throughput(if by_triples {
        Throughput::Elements(triples as u64)
    } else {
        Throughput::BytesDecimal(document.len() as u64)
    });
    group.bench_function("parse_turtle", |bencher| {
        bencher.iter(|| parse_turtle(black_box(&document), limits).expect("parses"));
    });
    group.bench_function("parse_turtle_terms", |bencher| {
        bencher.iter(|| parse_turtle_terms(black_box(&document), limits).expect("parses"));
    });
    group.bench_function("declared_prefixes", |bencher| {
        bencher.iter(|| declared_prefixes(black_box(&document), limits).expect("parses"));
    });
    group.finish();
}

criterion_group!(benches, reader_entry_points);
criterion_main!(benches);
