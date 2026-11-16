//! Local throughput benchmark; schema loading is outside the timed region.
use std::hint::black_box;
use std::time::{Duration, Instant};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

fn main() {
    let fields = (0..20)
        .map(|i| format!(r#"<xs:element name="f{i}" type="xs:string"/>"#))
        .collect::<String>();
    let source = format!(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
      <xs:complexType name="row"><xs:sequence>{fields}</xs:sequence></xs:complexType>
      <xs:element name="rows"><xs:complexType><xs:sequence>
        <xs:element name="row" type="row" maxOccurs="unbounded"/>
      </xs:sequence></xs:complexType></xs:element></xs:schema>"#
    );
    let schema = SchemaSet::load_bytes(source.as_bytes(), ParseOptions::default()).unwrap();
    let fields = (0..20)
        .map(|i| format!("<f{i}>value</f{i}>"))
        .collect::<String>();
    let document = format!(
        "<rows>{}</rows>",
        format!("<row>{fields}</row>").repeat(1000)
    );
    let run = || {
        let findings = schema
            .validate(black_box(document.as_bytes()), ValidateOptions::default())
            .unwrap();
        assert!(findings.is_empty(), "{findings:?}");
        black_box(findings);
    };
    for _ in 0..5 {
        run();
    }
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed() < Duration::from_millis(200) {
            run();
            iterations += 1;
        }
        samples.push(start.elapsed().as_secs_f64() / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "XSD 1000 rows x 20 fields: {:.1} us/iteration, {:.1} MiB/s (median of 7)",
        samples[3] * 1e6,
        document.len() as f64 / samples[3] / 1048576.0
    );
}
