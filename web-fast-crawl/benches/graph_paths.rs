use std::time::{Duration, Instant};
use web_fast_crawl::graph::{GraphBuilder, GraphLimits, Term};
fn main() {
    let mut builder = GraphBuilder::new(GraphLimits::default());
    for i in 0..100_000 {
        builder
            .insert(
                Term::iri(format!("https://e.test/{i}")),
                Term::iri("https://schema.org/relatedLink"),
                Term::iri(format!("https://e.test/{}", i + 1)),
                None,
                Term::iri("https://e.test/"),
            )
            .unwrap();
    }
    let graph = builder.finish();
    let node = graph.find(&Term::iri("https://e.test/50000")).unwrap();
    for indexed in [false, true] {
        let mut samples = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let mut n = 0;
            while start.elapsed() < Duration::from_millis(200) {
                let query = std::hint::black_box(node);
                let count = if indexed {
                    graph
                        .outgoing(query)
                        .fold(0u64, |sum, edge| sum + edge.object as u64)
                } else {
                    graph
                        .edges()
                        .iter()
                        .filter(|e| e.subject == query)
                        .fold(0u64, |sum, edge| sum + edge.object as u64)
                };
                std::hint::black_box(count);
                n += 1;
            }
            samples.push(start.elapsed().as_secs_f64() * 1e9 / n as f64);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{}: {:.1} ns/query",
            if indexed {
                "CSR outgoing"
            } else {
                "Full edge scan"
            },
            samples[2]
        );
    }
}
