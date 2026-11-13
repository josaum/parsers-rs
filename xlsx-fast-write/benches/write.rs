//! Throughput of the workbook writer, in cells per second.
//!
//! Run with `cargo bench -p xlsx-fast-write --bench write`.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use std::io::Cursor;
use xlsx_fast_write::Workbook;

const ROWS: u32 = 20_000;
const COLS: u16 = 10;

/// Builds a sheet of `ROWS` by `COLS` cells, half text and half numbers.
fn workbook() -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    for row in 0..ROWS {
        for col in 0..COLS {
            if col % 2 == 0 {
                sheet.write(row, col, format!("row {row} & column {col}"));
            } else {
                sheet.write(row, col, f64::from(row) * 1.5 + f64::from(col));
            }
        }
    }
    workbook
}

/// The same sheet as a JSON spec, the form the bindings pass in.
fn spec() -> String {
    let mut spec = String::from(r#"{"sheets":[{"name":"Data","rows":["#);
    for row in 0..ROWS {
        if row > 0 {
            spec.push(',');
        }
        spec.push('[');
        for col in 0..COLS {
            if col > 0 {
                spec.push(',');
            }
            if col % 2 == 0 {
                spec.push_str(&format!(r#"{{"t":"s","v":"row {row} & column {col}"}}"#));
            } else {
                spec.push_str(&format!(r#"{{"t":"n","v":{}}}"#, f64::from(row) * 1.5));
            }
        }
        spec.push(']');
    }
    spec.push_str("]}]}");
    spec
}

fn bench(criterion: &mut Criterion) {
    let cells = u64::from(ROWS) * u64::from(COLS);
    let mut group = criterion.benchmark_group("xlsx_write");
    group.throughput(Throughput::Elements(cells));
    group.sample_size(20);

    group.bench_function("build_and_write", |bencher| {
        bencher.iter(|| {
            let mut buffer = Cursor::new(Vec::new());
            workbook().write_to(&mut buffer).expect("writes");
            black_box(buffer.into_inner().len())
        });
    });
    let built = workbook();
    group.bench_function("write_only", |bencher| {
        bencher.iter(|| {
            let mut buffer = Cursor::new(Vec::new());
            built.write_to(&mut buffer).expect("writes");
            black_box(buffer.into_inner().len())
        });
    });
    let spec = spec();
    group.bench_function("json_spec_to_bytes", |bencher| {
        bencher.iter(|| {
            black_box(
                Workbook::write_json_spec_to_bytes(&spec)
                    .expect("writes")
                    .len(),
            )
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
