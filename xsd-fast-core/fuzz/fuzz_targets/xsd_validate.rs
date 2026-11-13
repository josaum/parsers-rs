#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// The schema the smoke tests load.
const SCHEMA: &[u8] = include_bytes!("../seeds/xsd_schema/schema.xsd");

// Any bytes as an XML instance validated against the seed schema.
fuzz_target!(|data: &[u8]| {
    static LOADED: OnceLock<SchemaSet> = OnceLock::new();
    let schema = LOADED.get_or_init(|| {
        SchemaSet::load_bytes(SCHEMA, ParseOptions::default()).expect("seed schema loads")
    });
    let _ = schema.validate(data, ValidateOptions::default());
    let _ = schema.validate(
        data,
        ValidateOptions {
            max_errors: 1,
            check_facets: true,
            check_cardinality: true,
        },
    );
});
