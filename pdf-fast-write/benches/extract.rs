//! Throughput of page extraction, in source bytes per second.
//!
//! Run with `cargo bench -p pdf-fast-write`.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use pdf_fast_write::PdfWriter;
use std::hint::black_box;

const PAGES: u32 = 500;

/// Builds a classic-xref document of `PAGES` pages, each with a content
/// stream of about 4 KiB and a shared font.
fn document() -> Vec<u8> {
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = String::new();
    for page in 0..PAGES {
        let page_number = 4 + 2 * page;
        kids.push_str(&format!("{page_number} 0 R "));
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                page_number + 1
            )
            .into_bytes(),
        );
        let line = format!(
            "BT /F1 12 Tf 72 720 Td (Page {} line of text) Tj ET\n",
            page + 1
        );
        let content = line.repeat(80);
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
            .into_bytes(),
        );
    }
    objects[1] = format!("<< /Type /Pages /Count {PAGES} /Kids [{kids}] >>").into_bytes();

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn bench(criterion: &mut Criterion) {
    let source = document();
    let all: Vec<usize> = (1..=PAGES as usize).collect();
    let ten: Vec<usize> = (1..=10).map(|n| n * 40).collect();
    let mut group = criterion.benchmark_group("pdf_extract");
    group.throughput(Throughput::Bytes(source.len() as u64));
    group.sample_size(20);
    group.bench_function("all_500_pages", |bencher| {
        bencher.iter(|| {
            black_box(
                PdfWriter::extract_pages_from_bytes(&source, &all)
                    .expect("extracts")
                    .len(),
            )
        });
    });
    group.bench_function("10_of_500_pages", |bencher| {
        bencher.iter(|| {
            black_box(
                PdfWriter::extract_pages_from_bytes(&source, &ten)
                    .expect("extracts")
                    .len(),
            )
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
