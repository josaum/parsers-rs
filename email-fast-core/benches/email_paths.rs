use email_fast_core::{parse_eml_bytes, ParseOptions};
use std::time::{Duration, Instant};
fn main() {
    let input=b"From: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nSubject: =?UTF-8?Q?Email_benchmark?=\r\nDate: Mon, 15 Jan 2024 14:30:00 +0000\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\nHello world\r\n";
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let mut count = 0;
        while start.elapsed() < Duration::from_millis(200) {
            std::hint::black_box(
                parse_eml_bytes(std::hint::black_box(input), ParseOptions::default()).unwrap(),
            );
            count += 1;
        }
        samples.push(start.elapsed().as_secs_f64() * 1e6 / count as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("EML median {:.3} us/message", samples[3]);
}
