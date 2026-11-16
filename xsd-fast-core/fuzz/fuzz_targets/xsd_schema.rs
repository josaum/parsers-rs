#![no_main]

use libfuzzer_sys::fuzz_target;
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

/// The instance document the smoke tests validate.
const INSTANCE: &[u8] = include_bytes!("../seeds/xsd_validate/instance.xml");

// Any bytes as a schema document; a schema that loads then validates the
// seed instance, so the compiled model is walked as well as built.
fuzz_target!(|data: &[u8]| {
    if let Ok(schema) = SchemaSet::load_bytes(data, ParseOptions::default()) {
        let _ = schema.validate(INSTANCE, ValidateOptions::default());
        let _ = serde_json::to_string(schema.model());
    }
});
