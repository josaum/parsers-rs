#![no_main]

use geo_fast_core::geojson::features_to_geojson;
use geo_fast_core::shapefile::parse_shapefile_bytes;
use libfuzzer_sys::fuzz_target;

/// The attribute table and the geometry file the smoke tests build.
const DBF: &[u8] = include_bytes!("../seeds/shapefile/table.dbf");
const SHP: &[u8] = include_bytes!("../seeds/shapefile/plain.shp");

// The input as a `.shp` file (alone and with the seed table), then as the
// `.dbf` table of the seed geometry.
fuzz_target!(|data: &[u8]| {
    if let Ok(parsed) = parse_shapefile_bytes(data, None) {
        let _ = features_to_geojson(&parsed.features);
    }
    let _ = parse_shapefile_bytes(data, Some(DBF));
    if let Ok(parsed) = parse_shapefile_bytes(SHP, Some(data)) {
        let _ = features_to_geojson(&parsed.features);
    }
});
