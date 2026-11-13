//! Thin PyO3 bindings for `geo-fast-core`.
//!
//! Surfaces the Geospatial helpers: GeoJSON / shapefile parse → WKT,
//! geodesic bbox IoU, antimeridian-aware bbox split, and STRtree queries.
//! Returns JSON strings so the Python control plane stays dependency-light.

#![deny(missing_docs)]

use geo_fast_core::hyperspectral::HyperspectralParser;
use geo_fast_core::{
    bbox_area_km2, bbox_iou_geodesic, parse_geojson_str, parse_geotiff_bands,
    parse_geotiff_bands_with_limit, parse_geotiff_geokey_params, parse_geotiff_metadata,
    parse_shapefile, BBox, GeoError, SpatialIndex,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use serde_json::json;
use std::collections::BTreeMap;

fn err(e: impl ToString) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Crate / binding version string.
#[pyfunction]
fn version() -> &'static str {
    geo_fast_core::VERSION
}

/// Parse a GeoJSON string and return a JSON list of `{id, wkt, lon, lat, properties}`.
#[pyfunction]
fn parse_geojson_to_records_json(py: Python<'_>, geojson: &str) -> PyResult<String> {
    py.detach(|| -> PyResult<String> {
        let features = parse_geojson_str(geojson).map_err(err)?;
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
    })
}

/// Parse a `.shp` path (companion `.dbf` inferred) → same record JSON shape.
#[pyfunction]
fn parse_shapefile_to_records_json(py: Python<'_>, path: &str) -> PyResult<String> {
    py.detach(|| -> PyResult<String> {
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
    })
}

/// Spherical geodesic IoU for two WMS-style bbox strings
/// (`min_lon,min_lat,max_lon,max_lat`). Antimeridian-aware.
#[pyfunction]
fn bbox_iou_geodesic_py(py: Python<'_>, a: &str, b: &str) -> PyResult<f64> {
    py.detach(|| -> PyResult<f64> {
        let ba = BBox::from_wms_string(a).map_err(err)?;
        let bb = BBox::from_wms_string(b).map_err(err)?;
        Ok(bbox_iou_geodesic(&ba, &bb))
    })
}

/// Spherical geodesic area (km²) for a WMS-style bbox string.
#[pyfunction]
fn bbox_area_km2_py(py: Python<'_>, bbox: &str) -> PyResult<f64> {
    py.detach(|| -> PyResult<f64> {
        let b = BBox::from_wms_string(bbox).map_err(err)?;
        // Split antimeridian so wrapping boxes still report area.
        let parts = b.split_antimeridian();
        Ok(parts.iter().map(bbox_area_km2).sum())
    })
}

/// Split an antimeridian-crossing WMS bbox into JSON `[[min_lon,min_lat,max_lon,max_lat], ...]`.
#[pyfunction]
fn split_bbox_antimeridian_json(py: Python<'_>, bbox: &str) -> PyResult<String> {
    py.detach(|| -> PyResult<String> {
        let b = BBox::from_wms_string(bbox).map_err(err)?;
        let parts: Vec<[f64; 4]> = b
            .split_antimeridian()
            .into_iter()
            .map(|p| p.to_array())
            .collect();
        serde_json::to_string(&parts).map_err(err)
    })
}

/// Bulk-load STRtree from JSON `[[min_lon,min_lat,max_lon,max_lat], ...]` and
/// query with a WMS bbox; returns JSON list of matching indices.
#[pyfunction]
fn strtree_query_json(py: Python<'_>, boxes_json: &str, query_bbox: &str) -> PyResult<String> {
    py.detach(|| -> PyResult<String> {
        let boxes: Vec<[f64; 4]> = serde_json::from_str(boxes_json).map_err(err)?;
        let items = boxes
            .into_iter()
            .enumerate()
            .map(|(i, a)| (BBox::new(a[0], a[1], a[2], a[3]), i));
        let idx = SpatialIndex::bulk_load(items);
        let q = BBox::from_wms_string(query_bbox).map_err(err)?;
        let mut hits = idx.query_antimeridian_aware(&q);
        hits.sort_unstable();
        serde_json::to_string(&hits).map_err(err)
    })
}

/// Build the JSON description of a GeoTIFF's georeferencing.
///
/// Keys are sorted. A non-finite number has no JSON form and is written as
/// `null`.
fn geotiff_metadata_value(path: &str) -> Result<serde_json::Value, GeoError> {
    let meta = parse_geotiff_metadata(path)?;
    let params = parse_geotiff_geokey_params(path)?;
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

/// Read a GeoTIFF's georeferencing and return it as a JSON object.
///
/// Keys: `width`, `height`, `band_count`, `bbox` (degrees, geographic CRS
/// only), `pixel_scale`, `tiepoints`, `transformation_matrix`, `epsg_code`,
/// `is_geographic`, `pixel_is_point`, `upper_left`, `lower_right`, `geokeys`
/// and `geokey_params` (`doubles` and `strings`, by GeoKey ID).
#[pyfunction]
fn geotiff_metadata_json(py: Python<'_>, path: &str) -> PyResult<String> {
    py.detach(|| -> PyResult<String> {
        let value = geotiff_metadata_value(path).map_err(err)?;
        serde_json::to_string(&value).map_err(err)
    })
}

/// Read every band of a GeoTIFF as a list of `float` lists, one per band.
///
/// `max_band_bytes` is the most band data to return, in bytes (four per
/// sample, all bands together); without it the core's default limit applies.
#[pyfunction]
#[pyo3(signature = (path, max_band_bytes=None))]
fn geotiff_bands_py(
    py: Python<'_>,
    path: &str,
    max_band_bytes: Option<usize>,
) -> PyResult<Vec<Vec<f32>>> {
    py.detach(|| -> PyResult<Vec<Vec<f32>>> {
        match max_band_bytes {
            Some(limit) => parse_geotiff_bands_with_limit(path, limit),
            None => parse_geotiff_bands(path),
        }
        .map_err(err)
    })
}

/// NDVI per sample from near-infrared and red bands of equal length.
#[pyfunction]
fn compute_ndvi_py(py: Python<'_>, nir_band: Vec<f32>, red_band: Vec<f32>) -> PyResult<Vec<f32>> {
    py.detach(|| HyperspectralParser::try_compute_ndvi(&nir_band, &red_band).map_err(err))
}

/// Spectral angle, in radians, between a pixel spectrum and a reference.
#[pyfunction]
fn spectral_angle_py(
    py: Python<'_>,
    pixel_spectrum: Vec<f32>,
    reference: Vec<f32>,
) -> PyResult<f32> {
    py.detach(|| HyperspectralParser::try_spectral_angle(&pixel_spectrum, &reference).map_err(err))
}

/// Python module `_core` — geo-fast GIS helpers.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(parse_geojson_to_records_json, m)?)?;
    m.add_function(wrap_pyfunction!(parse_shapefile_to_records_json, m)?)?;
    m.add_function(wrap_pyfunction!(bbox_iou_geodesic_py, m)?)?;
    m.add_function(wrap_pyfunction!(bbox_area_km2_py, m)?)?;
    m.add_function(wrap_pyfunction!(split_bbox_antimeridian_json, m)?)?;
    m.add_function(wrap_pyfunction!(strtree_query_json, m)?)?;
    m.add_function(wrap_pyfunction!(geotiff_metadata_json, m)?)?;
    m.add_function(wrap_pyfunction!(geotiff_bands_py, m)?)?;
    m.add_function(wrap_pyfunction!(compute_ndvi_py, m)?)?;
    m.add_function(wrap_pyfunction!(spectral_angle_py, m)?)?;
    Ok(())
}
