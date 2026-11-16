#![no_main]

use geo_fast_core::geojson::{features_to_geojson, parse_geojson_str};
use libfuzzer_sys::fuzz_target;

// Any text as GeoJSON; what parses is written back out.
fuzz_target!(|data: &[u8]| {
    if let Ok(features) = parse_geojson_str(&String::from_utf8_lossy(data)) {
        let _ = features_to_geojson(&features);
    }
});
