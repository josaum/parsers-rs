use std::time::{Duration, Instant};
use web_fast_core::{parse_html, ParseOptions};

fn main() {
    for count in [10, 1_000, 10_000] {
        let input = format!(
            "<main>{}</main>",
            "<p>Read <a href='/guide'>the guide</a>.</p>".repeat(count)
        );
        let mut samples = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let mut iterations = 0;
            while start.elapsed() < Duration::from_millis(200) {
                std::hint::black_box(
                    parse_html(
                        std::hint::black_box(&input),
                        Some("https://example.test/"),
                        ParseOptions::default(),
                    )
                    .unwrap(),
                );
                iterations += 1;
            }
            samples.push(start.elapsed().as_secs_f64() * 1e6 / iterations as f64);
        }
        samples.sort_by(f64::total_cmp);
        println!("{count} paragraphs: {:.3} us/document", samples[2]);
    }
}
