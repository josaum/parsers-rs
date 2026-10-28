//! Thin PyO3 bindings for `geo-fast-core`.
//!
//! Surfaces the Geospatial helpers: GeoJSON / shapefile parse → WKT,
//! geodesic bbox IoU, antimeridian-aware bbox split, and STRtree queries.
//! Returns JSON strings so the Python control plane stays dependency-light.

#![deny(missing_docs)]

use geo_fast_core::{
    bbox_area_km2, bbox_iou_geodesic, parse_geojson_str, parse_shapefile, BBox, SpatialIndex,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use serde_json::json;

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
fn parse_geojson_to_records_json(geojson: &str) -> PyResult<String> {
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
}

/// Parse a `.shp` path (companion `.dbf` inferred) → same record JSON shape.
#[pyfunction]
fn parse_shapefile_to_records_json(path: &str) -> PyResult<String> {
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

/// Spherical geodesic IoU for two WMS-style bbox strings
/// (`min_lon,min_lat,max_lon,max_lat`). Antimeridian-aware.
#[pyfunction]
fn bbox_iou_geodesic_py(a: &str, b: &str) -> PyResult<f64> {
    let ba = BBox::from_wms_string(a).map_err(err)?;
    let bb = BBox::from_wms_string(b).map_err(err)?;
    Ok(bbox_iou_geodesic(&ba, &bb))
}

/// Spherical geodesic area (km²) for a WMS-style bbox string.
#[pyfunction]
fn bbox_area_km2_py(bbox: &str) -> PyResult<f64> {
    let b = BBox::from_wms_string(bbox).map_err(err)?;
    // Split antimeridian so wrapping boxes still report area.
    let parts = b.split_antimeridian();
    Ok(parts.iter().map(bbox_area_km2).sum())
}

/// Split an antimeridian-crossing WMS bbox into JSON `[[min_lon,min_lat,max_lon,max_lat], ...]`.
#[pyfunction]
fn split_bbox_antimeridian_json(bbox: &str) -> PyResult<String> {
    let b = BBox::from_wms_string(bbox).map_err(err)?;
    let parts: Vec<[f64; 4]> = b
        .split_antimeridian()
        .into_iter()
        .map(|p| p.to_array())
        .collect();
    serde_json::to_string(&parts).map_err(err)
}

/// Bulk-load STRtree from JSON `[[min_lon,min_lat,max_lon,max_lat], ...]` and
/// query with a WMS bbox; returns JSON list of matching indices.
#[pyfunction]
fn strtree_query_json(boxes_json: &str, query_bbox: &str) -> PyResult<String> {
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
    Ok(())
}
