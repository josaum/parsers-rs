//! geo-fast-node: N-API bindings mirroring `geo-fast-py` (full parity).
//!
//! Geospatial GeoJSON / shapefile parse → WKT, geodesic bbox IoU,
//! antimeridian-aware bbox split, and STRtree queries.

use geo_fast_core::geotiff::{parse_geotiff_geokey_params_reader, parse_geotiff_metadata_reader};
use geo_fast_core::hyperspectral::HyperspectralParser;
use geo_fast_core::{
    bbox_area_km2, bbox_iou_geodesic, parse_geojson_str, parse_geotiff_bands,
    parse_geotiff_bands_with_limit, parse_shapefile, BBox, GeoError, SpatialIndex,
};
use napi::bindgen_prelude::*;
use napi::{JsNumber, JsObject, JsTypedArray, JsUnknown, TypedArrayType};
use napi_derive::napi;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Seek;

fn err(e: impl ToString) -> Error {
    Error::from_reason(e.to_string())
}

/// Crate / binding version string.
#[napi(catch_unwind)]
pub fn version() -> &'static str {
    geo_fast_core::VERSION
}

/// Parse a GeoJSON string and return a JSON list of `{id, wkt, lon, lat, properties}`.
#[napi(catch_unwind)]
pub fn parse_geojson_to_records_json(geojson: String) -> Result<String> {
    let features = parse_geojson_str(&geojson).map_err(err)?;
    let records: Vec<_> = features
        .iter()
        .map(|f| {
            let (lon, lat) = f
                .geometry
                .representative_point()
                .map(|p| (p.lon, p.lat))
                .unwrap_or((0.0, 0.0));
            json!({
                "id": f.id,
                "wkt": f.geometry.to_wkt(),
                "lon": lon,
                "lat": lat,
                "properties": f.properties,
            })
        })
        .collect();
    serde_json::to_string(&records).map_err(err)
}

/// Parse a `.shp` path (companion `.dbf` inferred) and return record JSON.
#[napi(catch_unwind)]
pub fn parse_shapefile_to_records_json(path: String) -> Result<String> {
    let data = parse_shapefile(path).map_err(err)?;
    let records: Vec<_> = data
        .features
        .iter()
        .map(|f| {
            let (lon, lat) = f
                .geometry
                .representative_point()
                .map(|p| (p.lon, p.lat))
                .unwrap_or((0.0, 0.0));
            json!({
                "id": f.id,
                "wkt": f.geometry.to_wkt(),
                "lon": lon,
                "lat": lat,
                "properties": f.properties,
            })
        })
        .collect();
    serde_json::to_string(&records).map_err(err)
}

/// Spherical geodesic IoU for two WMS bbox strings (`min_lon,min_lat,max_lon,max_lat`).
#[napi(catch_unwind)]
pub fn bbox_iou_geodesic_js(a: String, b: String) -> Result<f64> {
    let ba = BBox::from_wms_string(&a).map_err(err)?;
    let bb = BBox::from_wms_string(&b).map_err(err)?;
    Ok(bbox_iou_geodesic(&ba, &bb))
}

/// Spherical geodesic area (km²) for a WMS bbox string.
#[napi(catch_unwind)]
pub fn bbox_area_km2_js(bbox: String) -> Result<f64> {
    let b = BBox::from_wms_string(&bbox).map_err(err)?;
    Ok(b.split_antimeridian().iter().map(bbox_area_km2).sum())
}

/// Split an antimeridian-crossing WMS bbox into JSON `[[min_lon,min_lat,max_lon,max_lat], ...]`.
#[napi(catch_unwind)]
pub fn split_bbox_antimeridian_json(bbox: String) -> Result<String> {
    let b = BBox::from_wms_string(&bbox).map_err(err)?;
    let parts: Vec<[f64; 4]> = b
        .split_antimeridian()
        .into_iter()
        .map(|p| p.to_array())
        .collect();
    serde_json::to_string(&parts).map_err(err)
}

/// Bulk-load STRtree from JSON `[[min_lon,min_lat,max_lon,max_lat], ...]` and query with a WMS bbox.
#[napi(catch_unwind)]
pub fn strtree_query_json(boxes_json: String, query_bbox: String) -> Result<String> {
    let boxes: Vec<[f64; 4]> = serde_json::from_str(&boxes_json).map_err(err)?;
    let items = boxes
        .into_iter()
        .enumerate()
        .map(|(i, a)| (BBox::new(a[0], a[1], a[2], a[3]), i));
    let idx = SpatialIndex::bulk_load(items);
    let q = BBox::from_wms_string(&query_bbox).map_err(err)?;
    let mut hits = idx.query_antimeridian_aware(&q);
    hits.sort_unstable();
    serde_json::to_string(&hits).map_err(err)
}

/// Build the JSON description of a GeoTIFF's georeferencing.
///
/// Keys are sorted. A non-finite number has no JSON form and is written as
/// `null`.
fn geotiff_metadata_value(path: &str) -> std::result::Result<serde_json::Value, GeoError> {
    // One open file serves both reads; only the tags are read, never the
    // raster, so a large image costs no more than a small one.
    let file = std::fs::File::open(path).map_err(GeoError::Io)?;
    let mut reader = std::io::BufReader::new(file);
    let meta = parse_geotiff_metadata_reader(&mut reader)?;
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(GeoError::Io)?;
    let params = parse_geotiff_geokey_params_reader(&mut reader)?;
    let geokeys: BTreeMap<String, u32> = meta
        .geokeys
        .iter()
        .map(|(key, value)| (key.to_string(), *value))
        .collect();
    let doubles: BTreeMap<String, &Vec<f64>> = params
        .doubles
        .iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    let strings: BTreeMap<String, &String> = params
        .strings
        .iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    Ok(json!({
        "width": meta.width,
        "height": meta.height,
        "band_count": meta.band_count,
        "bbox": meta.bbox.as_ref().map(BBox::to_array),
        "pixel_scale": meta.pixel_scale.map(|(x, y)| [x, y]),
        "tiepoints": meta.tiepoints,
        "transformation_matrix": meta.transformation_matrix.map(|m| m.to_vec()),
        "epsg_code": meta.epsg_code,
        "is_geographic": meta.is_geographic,
        "pixel_is_point": meta.pixel_is_point(),
        "upper_left": meta.upper_left().map(|(x, y)| [x, y]),
        "lower_right": meta.lower_right().map(|(x, y)| [x, y]),
        "geokeys": geokeys,
        "geokey_params": { "doubles": doubles, "strings": strings },
    }))
}

/// Read a GeoTIFF's georeferencing and return it as a JSON object string.
///
/// Keys: `width`, `height`, `band_count`, `bbox`, `pixel_scale`, `tiepoints`,
/// `transformation_matrix`, `epsg_code`, `is_geographic`, `pixel_is_point`,
/// `upper_left`, `lower_right`, `geokeys` and `geokey_params`.
#[napi(catch_unwind)]
pub fn geotiff_metadata_json(path: String) -> Result<String> {
    let value = geotiff_metadata_value(&path).map_err(err)?;
    serde_json::to_string(&value).map_err(err)
}

/// Convert a JavaScript byte limit to `usize`, refusing what `u32` would wrap.
fn byte_limit(name: &str, value: f64) -> Result<usize> {
    // 2^53 is the largest count a JavaScript number holds exactly.
    const MAX_SAFE: f64 = 9_007_199_254_740_992.0;
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > MAX_SAFE {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{name} must be a non-negative whole number of bytes, got {value}"),
        ));
    }
    usize::try_from(value as u64).map_err(|_| {
        Error::new(
            Status::InvalidArg,
            format!("{name} is larger than this platform can address: {value}"),
        )
    })
}

/// Read samples from a `Float32Array`, a `Float64Array` or an array of numbers.
fn samples(name: &str, value: JsUnknown) -> Result<Vec<f32>> {
    if value.is_typedarray()? {
        // SAFETY: `is_typedarray` has just confirmed the value's type.
        let array = unsafe { value.cast::<JsTypedArray>() }.into_value()?;
        return match array.typedarray_type {
            TypedArrayType::Float32 => Ok(AsRef::<[f32]>::as_ref(&array).to_vec()),
            TypedArrayType::Float64 => Ok(AsRef::<[f64]>::as_ref(&array)
                .iter()
                .map(|v| *v as f32)
                .collect()),
            _ => Err(Error::new(
                Status::InvalidArg,
                format!("{name} must be a Float32Array, a Float64Array or an array of numbers"),
            )),
        };
    }
    if value.is_array()? {
        // SAFETY: `is_array` has just confirmed the value is an array object.
        let array = unsafe { value.cast::<JsObject>() };
        let len = array.get_array_length()?;
        let mut out = Vec::with_capacity(len as usize);
        for index in 0..len {
            out.push(array.get_element::<JsNumber>(index)?.get_double()? as f32);
        }
        return Ok(out);
    }
    Err(Error::new(
        Status::InvalidArg,
        format!("{name} must be a Float32Array, a Float64Array or an array of numbers"),
    ))
}

/// Read every band of a GeoTIFF, one `Float32Array` per band.
///
/// `max_band_bytes` is the most band data to return, in bytes (four per
/// sample, all bands together); without it the core's default limit applies.
/// It must be a non-negative whole number.
#[napi(catch_unwind)]
pub fn geotiff_bands(path: String, max_band_bytes: Option<f64>) -> Result<Vec<Float32Array>> {
    let bands = match max_band_bytes {
        Some(limit) => parse_geotiff_bands_with_limit(&path, byte_limit("maxBandBytes", limit)?),
        None => parse_geotiff_bands(&path),
    }
    .map_err(err)?;
    Ok(bands.into_iter().map(Float32Array::new).collect())
}

/// NDVI per sample from near-infrared and red bands of equal length.
///
/// Each band is a `Float32Array` (what `geotiff_bands` returns), a
/// `Float64Array` or an array of numbers.
#[napi(catch_unwind)]
pub fn compute_ndvi(nir_band: JsUnknown, red_band: JsUnknown) -> Result<Float32Array> {
    let nir = samples("nirBand", nir_band)?;
    let red = samples("redBand", red_band)?;
    HyperspectralParser::try_compute_ndvi(&nir, &red)
        .map(Float32Array::new)
        .map_err(err)
}

/// Spectral angle, in radians, between a pixel spectrum and a reference.
///
/// Each spectrum is a `Float32Array`, a `Float64Array` or an array of numbers.
#[napi(catch_unwind)]
pub fn spectral_angle(pixel_spectrum: JsUnknown, reference: JsUnknown) -> Result<f64> {
    let pixel = samples("pixelSpectrum", pixel_spectrum)?;
    let reference = samples("reference", reference)?;
    HyperspectralParser::try_spectral_angle(&pixel, &reference)
        .map(f64::from)
        .map_err(err)
}
