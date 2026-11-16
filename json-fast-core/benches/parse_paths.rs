//! Reproducible local throughput comparison; no timing assertions in CI.
use std::hint::black_box;
use std::time::{Duration, Instant};

fn measure(name: &str, bytes: usize, mut run: impl FnMut()) {
    for _ in 0..10 {
        run();
    }
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed() < Duration::from_millis(150) {
            run();
            iterations += 1;
        }
        samples.push(start.elapsed().as_secs_f64() / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    let seconds = samples[samples.len() / 2];
    println!(
        "{name}: {:.1} us/iteration, {:.1} MiB/s (median of 7)",
        seconds * 1e6,
        bytes as f64 / seconds / (1024.0 * 1024.0)
    );
}

fn main() {
    let input = serde_json::json!({"values": (0..50_000).collect::<Vec<u64>>()}).to_string();
    measure("query array", input.len(), || {
        black_box(json_fast_core::query::<Vec<u64>>(black_box(&input), "/values").unwrap());
    });
    let lines = " {\"id\":42,\"ok\":true} \r\n".repeat(10_000);
    measure("JSONL records", lines.len(), || {
        black_box(
            json_fast_core::jsonl::parse_many::<serde_json::Value>(black_box(&lines)).unwrap(),
        );
    });
}
