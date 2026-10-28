//! geo-fast-node: N-API bindings mirroring `geo-fast-py` (full parity).
//!
//! Geospatial GeoJSON / shapefile parse → WKT, geodesic bbox IoU,
//! antimeridian-aware bbox split, and STRtree queries.

use geo_fast_core::{
    bbox_area_km2, bbox_iou_geodesic, parse_geojson_str, parse_shapefile, BBox, SpatialIndex,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::json;

fn err(e: impl ToString) -> Error {
    Error::from_reason(e.to_string())
}

/// Crate / binding version string.
#[napi]
pub fn version() -> &'static str {
    geo_fast_core::VERSION
}

/// Parse a GeoJSON string and return a JSON list of `{id, wkt, lon, lat, properties}`.
#[napi]
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
#[napi]
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
#[napi]
pub fn bbox_iou_geodesic_js(a: String, b: String) -> Result<f64> {
    let ba = BBox::from_wms_string(&a).map_err(err)?;
    let bb = BBox::from_wms_string(&b).map_err(err)?;
    Ok(bbox_iou_geodesic(&ba, &bb))
}

/// Spherical geodesic area (km²) for a WMS bbox string.
#[napi]
pub fn bbox_area_km2_js(bbox: String) -> Result<f64> {
    let b = BBox::from_wms_string(&bbox).map_err(err)?;
    Ok(b.split_antimeridian().iter().map(bbox_area_km2).sum())
}

/// Split an antimeridian-crossing WMS bbox into JSON `[[min_lon,min_lat,max_lon,max_lat], ...]`.
#[napi]
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
#[napi]
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
