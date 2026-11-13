//! GeoJSON (RFC 7946) serialization / deserialization.
//!
//! Converts between the crate's internal [`GeoFeature`] / [`Geometry`] types
//! and the standard GeoJSON wire format.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value};

use crate::error::GeoError;
use crate::types::{Coordinate, GeoFeature, Geometry};

// ── Wire types ───────────────────────────────────────────────────────────────

/// GeoJSON `FeatureCollection` wire representation.
#[derive(Debug, Serialize, Deserialize)]
pub struct GeoJsonFeatureCollection {
    /// GeoJSON object type; should be `"FeatureCollection"`.
    #[serde(rename = "type")]
    pub type_: String,
    /// Optional collection bounding box `[min_lon, min_lat, max_lon, max_lat]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f64; 4]>,
    /// Serialized GeoJSON feature objects.
    pub features: Vec<Value>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Serialize a slice of [`GeoFeature`] as a GeoJSON `FeatureCollection`.
pub fn features_to_geojson(features: &[GeoFeature]) -> Result<Value, GeoError> {
    let json_features: Vec<Value> = features
        .iter()
        .map(feature_to_geojson_value)
        .collect::<Result<_, _>>()?;

    Ok(json!({
        "type": "FeatureCollection",
        "features": json_features
    }))
}

/// Serialize a single [`GeoFeature`] as a GeoJSON `Feature`.
pub fn feature_to_geojson(feature: &GeoFeature) -> Result<Value, GeoError> {
    feature_to_geojson_value(feature)
}

/// Deserialize a GeoJSON `FeatureCollection` or `Feature` into [`GeoFeature`]s.
pub fn geojson_to_features(geojson: &Value) -> Result<Vec<GeoFeature>, GeoError> {
    let type_ = geojson
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| GeoError::InvalidFormat("Missing 'type' in GeoJSON".into()))?;

    match type_ {
        "FeatureCollection" => {
            let features = geojson
                .get("features")
                .and_then(Value::as_array)
                .ok_or_else(|| GeoError::InvalidFormat("Missing 'features' array".into()))?;
            features.iter().map(parse_feature).collect()
        }
        "Feature" => Ok(vec![parse_feature(geojson)?]),
        other => Err(GeoError::InvalidFormat(format!(
            "Unsupported GeoJSON type: {other}"
        ))),
    }
}

/// Deepest nesting of arrays and objects accepted in a GeoJSON document.
///
/// `serde_json` parses, clones, serializes and drops a value recursively and
/// stops only at 128 levels. In an unoptimized build one level costs a little
/// over 2 KiB of stack (64 levels were measured at about 148 KiB), so its own
/// limit sits at the edge of a 256 KiB worker stack and a hostile document
/// overflows it. The deepest standard GeoJSON structure is 8 levels
/// (FeatureCollection, `features`, Feature, `geometry`, then the four array
/// levels of MultiPolygon coordinates); 32 leaves 28 levels for structured
/// `properties` and uses under a third of that stack. Raising it needs the
/// small-stack test rerun; lowering it rejects documents with nested
/// `properties`.
const MAX_NESTING_DEPTH: usize = 32;

/// Parse a GeoJSON string into a list of features.
///
/// # Errors
///
/// Returns [`GeoError::InvalidFormat`] when `s` is not valid JSON, nests arrays
/// and objects deeper than 32 levels, or is not a GeoJSON `FeatureCollection`
/// or `Feature`, and [`GeoError::UnsupportedFormat`] for a geometry type this
/// crate does not read.
pub fn parse_geojson_str(s: &str) -> Result<Vec<GeoFeature>, GeoError> {
    check_nesting_depth(s.as_bytes())?;
    let v: Value = serde_json::from_str(s)
        .map_err(|e| GeoError::InvalidFormat(format!("JSON parse error: {e}")))?;
    geojson_to_features(&v)
}

/// Refuses a JSON document that nests deeper than [`MAX_NESTING_DEPTH`].
///
/// Run on the text before `serde_json` is given it, by
/// [`parse_geojson_str`] and by the WFS client, which reads its response as
/// bytes.
///
/// # Errors
///
/// Returns [`GeoError::InvalidFormat`] when arrays and objects in `json`
/// nest deeper than [`MAX_NESTING_DEPTH`] levels.
pub(crate) fn check_nesting_depth(json: &[u8]) -> Result<(), GeoError> {
    if exceeds_nesting_depth(json, MAX_NESTING_DEPTH) {
        return Err(GeoError::InvalidFormat(format!(
            "GeoJSON nests arrays and objects deeper than {MAX_NESTING_DEPTH} levels"
        )));
    }
    Ok(())
}

/// Returns whether arrays and objects in `json` nest deeper than `limit`.
///
/// A single pass that does not recurse and does not validate the text: it
/// tracks string literals so brackets inside them are not counted, and counts
/// opening brackets everywhere else. Up to the first syntax error a JSON
/// parser is never nested deeper than this count, which is all the limit
/// needs. Only ASCII bytes are looked at, so it reads UTF-8 as bytes.
fn exceeds_nesting_depth(json: &[u8], limit: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in json {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn feature_to_geojson_value(feature: &GeoFeature) -> Result<Value, GeoError> {
    let geometry = geometry_to_value(&feature.geometry)?;
    let id: Value = match &feature.id {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    };
    Ok(json!({
        "type": "Feature",
        "id": id,
        "geometry": geometry,
        "properties": feature.properties
    }))
}

fn geometry_to_value(geom: &Geometry) -> Result<Value, GeoError> {
    match geom {
        Geometry::Point(p) => Ok(json!({
            "type": "Point",
            "coordinates": [p.lon, p.lat]
        })),
        Geometry::MultiPoint(pts) => {
            let coords: Vec<Value> = pts.iter().map(|p| json!([p.lon, p.lat])).collect();
            Ok(json!({
                "type": "MultiPoint",
                "coordinates": coords
            }))
        }
        Geometry::Polyline(parts) => {
            if parts.len() == 1 {
                let ring: Vec<Value> = parts[0].iter().map(|p| json!([p.lon, p.lat])).collect();
                Ok(json!({
                    "type": "LineString",
                    "coordinates": ring
                }))
            } else {
                let rings: Vec<Value> = parts
                    .iter()
                    .map(|part| {
                        let coords: Vec<Value> =
                            part.iter().map(|p| json!([p.lon, p.lat])).collect();
                        Value::Array(coords)
                    })
                    .collect();
                Ok(json!({
                    "type": "MultiLineString",
                    "coordinates": rings
                }))
            }
        }
        Geometry::Polygon(rings) => {
            let json_rings: Vec<Value> = rings
                .iter()
                .map(|ring| {
                    let coords: Vec<Value> = ring.iter().map(|p| json!([p.lon, p.lat])).collect();
                    Value::Array(coords)
                })
                .collect();
            Ok(json!({
                "type": "Polygon",
                "coordinates": json_rings
            }))
        }
        Geometry::MultiPolygon(polys) => {
            let json_polys: Vec<Value> = polys
                .iter()
                .map(|poly| {
                    let rings: Vec<Value> = poly
                        .iter()
                        .map(|ring| {
                            let coords: Vec<Value> =
                                ring.iter().map(|p| json!([p.lon, p.lat])).collect();
                            Value::Array(coords)
                        })
                        .collect();
                    Value::Array(rings)
                })
                .collect();
            Ok(json!({
                "type": "MultiPolygon",
                "coordinates": json_polys
            }))
        }
    }
}

fn parse_feature(v: &Value) -> Result<GeoFeature, GeoError> {
    let id = v.get("id").and_then(|i| {
        if i.is_null() {
            None
        } else {
            Some(i.to_string().trim_matches('"').to_string())
        }
    });

    let props = v
        .get("properties")
        .cloned()
        .unwrap_or(Value::Object(JsonMap::new()));

    let geom_val = v
        .get("geometry")
        .ok_or_else(|| GeoError::InvalidFormat("Feature missing 'geometry'".into()))?;

    if geom_val.is_null() {
        return Err(GeoError::InvalidFormat(
            "Null geometry not supported".into(),
        ));
    }

    let geom = parse_geometry(geom_val)?;
    Ok(GeoFeature::new(id, geom, props))
}

fn parse_geometry(v: &Value) -> Result<Geometry, GeoError> {
    let type_ = v
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| GeoError::InvalidFormat("Geometry missing 'type'".into()))?;

    let coords = v
        .get("coordinates")
        .ok_or_else(|| GeoError::InvalidFormat("Geometry missing 'coordinates'".into()))?;

    match type_ {
        "Point" => {
            let p = parse_coord_pair(coords)?;
            Ok(Geometry::Point(p))
        }
        "MultiPoint" => {
            let pts = coords
                .as_array()
                .ok_or_else(|| {
                    GeoError::InvalidFormat("MultiPoint coordinates must be array".into())
                })?
                .iter()
                .map(parse_coord_pair)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Geometry::MultiPoint(pts))
        }
        "LineString" => {
            let ring = parse_ring(coords)?;
            Ok(Geometry::Polyline(vec![ring]))
        }
        "MultiLineString" => {
            let parts = coords
                .as_array()
                .ok_or_else(|| {
                    GeoError::InvalidFormat("MultiLineString coordinates must be array".into())
                })?
                .iter()
                .map(parse_ring)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Geometry::Polyline(parts))
        }
        "Polygon" => {
            let rings = coords
                .as_array()
                .ok_or_else(|| GeoError::InvalidFormat("Polygon coordinates must be array".into()))?
                .iter()
                .map(parse_ring)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Geometry::Polygon(rings))
        }
        "MultiPolygon" => {
            // Preserve per-polygon ring groups — do not flatten into one Polygon.
            // Flattening merged exteriors/holes across polygons and broke WKT /
            // ArcGIS round-trips for GIS map import.
            let polys = coords.as_array().ok_or_else(|| {
                GeoError::InvalidFormat("MultiPolygon coordinates must be array".into())
            })?;
            let mut out = Vec::with_capacity(polys.len());
            for poly in polys {
                let rings = poly.as_array().ok_or_else(|| {
                    GeoError::InvalidFormat("Polygon in MultiPolygon must be array".into())
                })?;
                let mut parsed_rings = Vec::with_capacity(rings.len());
                for ring in rings {
                    parsed_rings.push(parse_ring(ring)?);
                }
                out.push(parsed_rings);
            }
            Ok(Geometry::MultiPolygon(out))
        }
        other => Err(GeoError::UnsupportedFormat(format!(
            "Unsupported GeoJSON geometry type: {other}"
        ))),
    }
}

fn parse_coord_pair(v: &Value) -> Result<Coordinate, GeoError> {
    let arr = v
        .as_array()
        .ok_or_else(|| GeoError::InvalidFormat("Coordinate must be array".into()))?;
    if arr.len() < 2 {
        return Err(GeoError::InvalidFormat(
            "Coordinate array must have at least 2 elements".into(),
        ));
    }
    let lon = arr[0]
        .as_f64()
        .ok_or_else(|| GeoError::InvalidFormat("Longitude must be number".into()))?;
    let lat = arr[1]
        .as_f64()
        .ok_or_else(|| GeoError::InvalidFormat("Latitude must be number".into()))?;
    if !lon.is_finite() || !lat.is_finite() {
        return Err(GeoError::InvalidFormat(
            "Coordinate components must be finite".into(),
        ));
    }
    Ok(Coordinate::new(lon, lat))
}

fn parse_ring(v: &Value) -> Result<Vec<Coordinate>, GeoError> {
    v.as_array()
        .ok_or_else(|| GeoError::InvalidFormat("Ring must be array".into()))?
        .iter()
        .map(parse_coord_pair)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FEATURE_COLLECTION: &str = r#"{
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "id": "1",
                "geometry": {
                    "type": "Point",
                    "coordinates": [-47.9172, -15.7797]
                },
                "properties": { "name": "Brasília" }
            },
            {
                "type": "Feature",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[
                        [-48.0, -16.0], [-47.0, -16.0], [-47.0, -15.0],
                        [-48.0, -15.0], [-48.0, -16.0]
                    ]]
                },
                "properties": { "name": "área" }
            }
        ]
    }"#;

    #[test]
    fn test_roundtrip_feature_collection() {
        let features = parse_geojson_str(SAMPLE_FEATURE_COLLECTION).unwrap();
        assert_eq!(features.len(), 2);

        match &features[0].geometry {
            Geometry::Point(p) => {
                assert!((p.lon - (-47.9172)).abs() < 1e-9);
            }
            _ => panic!("Expected Point"),
        }

        match &features[1].geometry {
            Geometry::Polygon(rings) => {
                assert_eq!(rings.len(), 1);
                assert_eq!(rings[0].len(), 5);
            }
            _ => panic!("Expected Polygon"),
        }

        // Serialize back
        let out = features_to_geojson(&features).unwrap();
        let type_ = out["type"].as_str().unwrap();
        assert_eq!(type_, "FeatureCollection");
        let arr = out["features"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_linestring_parses_as_polyline() {
        let geojson = r#"{
            "type": "Feature",
            "geometry": {
                "type": "LineString",
                "coordinates": [[-47.0, -15.0], [-46.0, -14.0]]
            },
            "properties": {}
        }"#;
        let features = parse_geojson_str(geojson).unwrap();
        match &features[0].geometry {
            Geometry::Polyline(parts) => assert_eq!(parts.len(), 1),
            _ => panic!("Expected Polyline"),
        }
    }

    #[test]
    fn test_multipolygon_roundtrip_preserves_groups() {
        // WHY: flattening MultiPolygon into one Polygon merged exteriors across
        // features and produced invalid WKT for Milvus / ArcGIS import.
        let geojson = r#"{
            "type": "Feature",
            "geometry": {
                "type": "MultiPolygon",
                "coordinates": [
                    [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]],
                    [[[10.0, 10.0], [11.0, 10.0], [11.0, 11.0], [10.0, 10.0]]]
                ]
            },
            "properties": { "name": "two" }
        }"#;
        let features = parse_geojson_str(geojson).unwrap();
        match &features[0].geometry {
            Geometry::MultiPolygon(polys) => {
                assert_eq!(polys.len(), 2);
                assert_eq!(polys[0].len(), 1);
                assert_eq!(polys[1][0].len(), 4);
            }
            other => panic!("Expected MultiPolygon, got {other:?}"),
        }

        let out = features_to_geojson(&features).unwrap();
        let roundtrip = geojson_to_features(&out).unwrap();
        match &roundtrip[0].geometry {
            Geometry::MultiPolygon(polys) => assert_eq!(polys.len(), 2),
            other => panic!("Expected MultiPolygon after roundtrip, got {other:?}"),
        }
    }

    #[test]
    fn test_rejects_non_finite_coordinates() {
        let geojson = r#"{
            "type": "Feature",
            "geometry": { "type": "Point", "coordinates": [null, 1.0] },
            "properties": {}
        }"#;
        assert!(parse_geojson_str(geojson).is_err());
    }
}
