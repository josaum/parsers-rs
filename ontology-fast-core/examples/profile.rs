//! Measurement companion to the `turtle_reader` benchmark.
//!
//! ```text
//! cargo run --release -p ontology-fast-core --example profile -- dump
//! cargo run --release -p ontology-fast-core --example profile -- amp
//! /usr/bin/time -l target/release/examples/profile rss parse_turtle
//! ```
//!
//! * `dump` hashes everything the public API returns for the benchmark
//!   document, a set of hand-written documents and fixed-seed mutations of
//!   them. Two builds that print the same hashes return byte-identical output.
//! * `amp` reports peak heap divided by input size for the input shapes that
//!   cost the most memory inside the default limits.
//! * `rss <entry>` runs one entry point once over the benchmark document, for
//!   an external peak-resident-memory measurement.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;
#[path = "../benches/support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ontology_fast_core::{
    abbreviate, declared_prefixes, parse_turtle, parse_turtle_terms, predicate_text,
    OntologyLimits, TermView,
};

/// Counts live and peak heap bytes so `amp` can report peak memory per input byte.
struct PeakAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn charge(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call is forwarded unchanged to the system allocator; the
// counters only observe sizes.
unsafe impl GlobalAlloc for PeakAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `pointer` came from `System` with this layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        charge(new_size);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: PeakAllocator = PeakAllocator;

/// 64-bit FNV-1a over a stream of fields, each closed by a separator byte.
struct Digest(u64);

impl Digest {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn field(&mut self, text: &str) {
        for byte in text.bytes().chain([0xff]) {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn ordinals(&mut self, ordinals: &[u32]) {
        self.field(&ordinals.len().to_string());
        for ordinal in ordinals {
            self.field(&ordinal.to_string());
        }
    }
}

/// Hash everything the three entry points return for `source`.
///
/// Each distinct term is looked up once in every index column, and each
/// distinct pattern is selected once: a predicate shared by a million rows
/// answers with a million ordinals, and hashing that answer for every row
/// would be quadratic without checking anything more.
fn digest_document(digest: &mut Digest, source: &[u8], limits: OntologyLimits) {
    match parse_turtle(source, limits) {
        Err(error) => digest.field(&format!("text-error {} {error}", error.code())),
        Ok(ontology) => {
            for (prefix, namespace) in ontology.prefixes() {
                digest.field(prefix);
                digest.field(namespace);
            }
            let index = ontology.index();
            let mut probed: HashSet<&str> = HashSet::new();
            let mut selected: HashSet<[&str; 3]> = HashSet::new();
            for triple in ontology.triples() {
                digest.field(&triple.to_line());
                let (s, p, o) = (
                    triple.subject.as_str(),
                    triple.predicate.as_str(),
                    triple.object.as_str(),
                );
                // Every term is probed in every column: a subject that is not
                // an object must still answer with an empty slice.
                for term in [s, p, o] {
                    if probed.insert(term) {
                        digest.ordinals(index.by_subject(term));
                        digest.ordinals(index.by_predicate(term));
                        digest.ordinals(index.by_object(term));
                    }
                }
                let patterns = [
                    [Some(s), Some(p), None],
                    [None, Some(p), Some(o)],
                    [Some(s), None, Some(o)],
                    [Some(s), Some(p), Some(o)],
                    [Some(o), Some(p), Some(s)],
                ];
                for pattern in patterns {
                    if selected.insert(pattern.map(|term| term.unwrap_or("\u{1}any"))) {
                        digest.ordinals(&index.select(pattern[0], pattern[1], pattern[2]));
                    }
                }
            }
            digest.ordinals(index.by_subject("absent"));
            digest.ordinals(index.by_subject(""));
            digest.ordinals(&index.select(None, None, None));
        }
    }
    match parse_turtle_terms(source, limits) {
        Err(error) => digest.field(&format!("terms-error {} {error}", error.code())),
        Ok(triples) => {
            let prefixes = declared_prefixes(source, limits).unwrap_or_default();
            for triple in &triples {
                digest.field(&triple.subject);
                digest.field(&triple.predicate);
                digest.field(&abbreviate(&triple.subject, &prefixes));
                digest.field(&predicate_text(&triple.predicate, &prefixes));
                match &triple.object {
                    TermView::Iri(iri) => {
                        digest.field("iri");
                        digest.field(iri);
                        digest.field(&abbreviate(iri, &prefixes));
                    }
                    TermView::Blank(label) => {
                        digest.field("blank");
                        digest.field(label);
                    }
                    TermView::Literal {
                        value,
                        datatype,
                        language,
                    } => {
                        digest.field("literal");
                        digest.field(value);
                        digest.field(datatype.as_deref().unwrap_or("\u{1}none"));
                        digest.field(language.as_deref().unwrap_or("\u{1}none"));
                    }
                }
            }
        }
    }
    match declared_prefixes(source, limits) {
        Err(error) => digest.field(&format!("prefix-error {} {error}", error.code())),
        Ok(prefixes) => {
            for (prefix, namespace) in &prefixes {
                digest.field(prefix);
                digest.field(namespace);
            }
        }
    }
}

/// Hand-written documents covering what the crate's tests assert.
fn fixed_documents() -> Vec<Vec<u8>> {
    [
        concat!(
            "@prefix ex: <http://example.org/> .\n",
            "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
            "@base <http://example.org/base/> .\n",
            "ex:a a ex:C ; ex:p \"lit\"@en , \"3\"^^xsd:integer , 4.5e3 , true ;\n",
            "  ex:q [ ex:r ( 1 2 ex:b ) ] .\n",
            "<rel> ex:p \"\"\"multi\nline \\u00e9 \\\"q\\\"\"\"\" .\n",
            "_:b1 ex:p _:b2 . # comment\n",
        ),
        // A prefix bound twice, used under both bindings.
        "@prefix ex: <urn:one:> .\nex:a ex:p ex:b .\n@prefix ex: <urn:two:> .\nex:a ex:p ex:b .\n",
        // Nested namespaces, two prefixes for one namespace, and a late directive.
        concat!(
            "<urn:n:deep/x> <urn:n:p> <urn:n:deep/er/y> , <urn:other> .\n",
            "@prefix b: <urn:n:deep/> .\n@prefix a: <urn:n:deep/> .\n",
            "@prefix n: <urn:n:> .\nPREFIX er: <urn:n:deep/er/>\n@prefix : <urn:empty#> .\n",
            ":x a :Y ; n:p \"n:p\" , n:p , \"a\" , \"<urn:other>\" .\n",
        ),
        // A literal, an IRI and a blank node whose text forms coincide.
        concat!(
            "@prefix ex: <urn:x:> .\n",
            "ex:s ex:p \"ex:o\" , ex:o , \"ex:o\"@en , \"ex:o\"^^ex:t , \"ex:o\" .\n",
            "ex:s ex:p ex:o .\n[] ex:p [] , _:k .\n_:k ex:p ( _:k ( ) \"\" ) .\n",
            "ex:s ex:p \"_:k\" , \"x\"^^<http://www.w3.org/2001/XMLSchema#string> , \"x\" .\n",
        ),
        "<urn:a> <urn:p> \"x\"@en--ltr .\n",
        "<urn:a> <urn:p> <<( <urn:a> <urn:p> <urn:b> )>> .\n",
        "@prefix broken: <urn:x",
        "<urn:g> { <urn:a> <urn:p> <urn:b> . }\n",
        "",
    ]
    .iter()
    .map(|text| text.as_bytes().to_vec())
    .collect()
}

fn dump() {
    let limits = OntologyLimits::default();
    let document = support::synthetic_turtle(support::BENCH_DOCUMENT_BYTES);
    let mut digest = Digest::new();
    digest_document(&mut digest, &document, limits);
    println!("bench-document {:016x}", digest.0);
    drop(document);

    let fixed = fixed_documents();
    let mut digest = Digest::new();
    for document in &fixed {
        digest_document(&mut digest, document, limits);
        // The same documents under budgets tight enough to refuse some of them.
        for statements in [0, 1, 3, 7] {
            digest_document(
                &mut digest,
                document,
                OntologyLimits::with_overrides(None, Some(statements)),
            );
        }
        digest_document(
            &mut digest,
            document,
            OntologyLimits::with_overrides(Some(document.len().saturating_sub(1)), None),
        );
    }
    println!("fixed-documents {:016x}", digest.0);

    let mut digest = Digest::new();
    let mut rng = smoke::Rng::new(0x5eed);
    let mut parsed = 0_u32;
    for round in 0..40_000 {
        let mutated = smoke::mutate(&fixed[round % 4], &mut rng);
        parsed += u32::from(parse_turtle(&mutated, limits).is_ok());
        digest_document(&mut digest, &mutated, limits);
    }
    println!("mutations {:016x} ({parsed} of 40000 parse)", digest.0);
}

/// The input shapes that cost the most memory per source byte.
fn amplification_shapes() -> Vec<(&'static str, Vec<u8>)> {
    let mut shapes = Vec::new();

    let mut text = String::from("<a:a> <a:b> 0");
    for n in 1..2_000_000 {
        let _ = write!(text, ",{n}");
    }
    text.push('.');
    shapes.push(("2M short triples (object list)", text.into_bytes()));

    let mut text = String::from("<a:a> <a:b> \"");
    text.push_str(&"x".repeat(60 * 1024 * 1024));
    text.push_str("\" .");
    shapes.push(("one 60 MiB literal", text.into_bytes()));

    let mut text = String::from("<a:a> <a:b> (");
    text.push_str(&"1 ".repeat(999_990));
    text.push_str(").");
    shapes.push(("collection of 1M items (2M triples)", text.into_bytes()));

    let mut text = String::from("<a:a> <a:b> ");
    text.push_str(&"(".repeat(999_990));
    text.push_str(&")".repeat(999_990));
    text.push('.');
    shapes.push(("collections nested 1M deep", text.into_bytes()));

    let mut text = String::from("<a:a> <a:b> ");
    text.push_str(&"[<a:b> ".repeat(1_999_990));
    text.push('1');
    text.push_str(&"]".repeat(1_999_990));
    text.push('.');
    shapes.push((
        "blank node property lists nested 2M deep",
        text.into_bytes(),
    ));

    let mut text = String::new();
    for n in 0..1_000_000 {
        let _ = writeln!(text, "@prefix p{n}:<a:{n}/>.");
    }
    text.push_str("<a:7/x> <a:999999/y> <a:b> .");
    shapes.push(("1M prefix declarations", text.into_bytes()));

    shapes
}

fn amp() {
    let limits = OntologyLimits::default();
    println!("shape | bytes | entry | result | peak heap MiB | peak/input | seconds");
    for (name, source) in amplification_shapes() {
        type Entry = fn(&[u8], OntologyLimits) -> String;
        let entries: [(&str, Entry); 3] = [
            ("parse_turtle", |s, l| match parse_turtle(s, l) {
                Ok(ontology) => format!("{} triples", ontology.triples().len()),
                Err(error) => error.code().to_owned(),
            }),
            ("parse_turtle_terms", |s, l| {
                match parse_turtle_terms(s, l) {
                    Ok(triples) => format!("{} triples", triples.len()),
                    Err(error) => error.code().to_owned(),
                }
            }),
            ("declared_prefixes", |s, l| match declared_prefixes(s, l) {
                Ok(prefixes) => format!("{} prefixes", prefixes.len()),
                Err(error) => error.code().to_owned(),
            }),
        ];
        for (entry, run) in entries {
            let before = LIVE.load(Ordering::Relaxed);
            PEAK.store(before, Ordering::Relaxed);
            let started = Instant::now();
            let outcome = run(&source, limits);
            let seconds = started.elapsed().as_secs_f64();
            let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
            println!(
                "{name} | {} | {entry} | {outcome} | {:.1} | {:.1}x | {seconds:.2}",
                source.len(),
                peak as f64 / (1024.0 * 1024.0),
                peak as f64 / source.len() as f64,
            );
        }
    }
}

fn rss(entry: &str) {
    let limits = OntologyLimits::default();
    let document = support::synthetic_turtle(support::BENCH_DOCUMENT_BYTES);
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let started = Instant::now();
    let rows = match entry {
        "parse_turtle" => parse_turtle(&document, limits).map(|o| o.triples().len()),
        "parse_turtle_terms" => parse_turtle_terms(&document, limits).map(|t| t.len()),
        _ => declared_prefixes(&document, limits).map(|p| p.len()),
    };
    let seconds = started.elapsed().as_secs_f64();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    println!(
        "{entry}: {rows:?} rows, {} input bytes, {seconds:.2} s, peak heap above input {:.1} MiB",
        document.len(),
        peak as f64 / (1024.0 * 1024.0)
    );
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("dump") => dump(),
        Some("amp") => amp(),
        Some("rss") => rss(arguments.get(1).map_or("parse_turtle", String::as_str)),
        _ => eprintln!(
            "usage: profile dump | amp | rss <parse_turtle|parse_turtle_terms|declared_prefixes>"
        ),
    }
}
