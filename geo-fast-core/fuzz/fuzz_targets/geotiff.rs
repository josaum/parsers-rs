#![no_main]

use geo_fast_core::geotiff::parse_geotiff_metadata_reader;
use geo_fast_core::thermal::ThermalParser;
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

// Any bytes as a TIFF: the GeoTIFF metadata reader, then the thermal raster
// reader, which decodes the pixel data.
fuzz_target!(|data: &[u8]| {
    if let Ok(metadata) = parse_geotiff_metadata_reader(Cursor::new(data)) {
        let _ = metadata.upper_left();
        let _ = metadata.lower_right();
    }
    let _ = ThermalParser::parse_thermal_tiff(data);
});
