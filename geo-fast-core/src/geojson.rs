//! GeoJSON (RFC 7946) serialization / deserialization.
//!
//! Converts between the crate's internal [`GeoFeature`] / [`Geometry`] types
//! and the standard GeoJSON wire format.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// Parse a GeoJSON string into a list of features.
pub fn parse_geojson_str(s: &str) -> Result<Vec<GeoFeature>, GeoError> {
    let v: Value = serde_json::from_str(s)
        .map_err(|e| GeoError::InvalidFormat(format!("JSON parse error: {e}")))?;
    geojson_to_features(&v)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn feature_to_geojson_value(feature: &GeoFeature) -> Result<Value, GeoError> {
    let geometry = geometry_to_value(&feature.geometry)?;
    if !(feature.properties.is_object() || feature.properties.is_null()) {
        return Err(GeoError::InvalidFormat(
            "Feature properties must be an object or null".into(),
        ));
    }
    let mut value = json!({
        "type": "Feature",
        "geometry": geometry,
        "properties": feature.properties
    });
    if let Some(id) = &feature.id {
        value["id"] = Value::String(id.clone());
    }
    Ok(value)
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
    if v.get("type").and_then(Value::as_str) != Some("Feature") {
        return Err(GeoError::InvalidFormat(
            "FeatureCollection members must be Features".into(),
        ));
    }
    let id = match v.get("id") {
        None => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => {
            return Err(GeoError::InvalidFormat(
                "Feature id must be a string or number".into(),
            ))
        }
    };
    let props = v
        .get("properties")
        .filter(|value| value.is_object() || value.is_null())
        .ok_or_else(|| {
            GeoError::InvalidFormat("Feature requires object or null properties".into())
        })?
        .clone();

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
            let ring = parse_line(coords)?;
            Ok(Geometry::Polyline(vec![ring]))
        }
        "MultiLineString" => {
            let parts = coords
                .as_array()
                .ok_or_else(|| {
                    GeoError::InvalidFormat("MultiLineString coordinates must be array".into())
                })?
                .iter()
                .map(parse_line)
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
    if arr
        .iter()
        .any(|value| !value.as_f64().is_some_and(f64::is_finite))
    {
        return Err(GeoError::InvalidFormat(
            "Every coordinate component must be a finite number".into(),
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

fn parse_positions(v: &Value) -> Result<Vec<Coordinate>, GeoError> {
    v.as_array()
        .ok_or_else(|| GeoError::InvalidFormat("Coordinates must be an array".into()))?
        .iter()
        .map(parse_coord_pair)
        .collect()
}

fn parse_line(v: &Value) -> Result<Vec<Coordinate>, GeoError> {
    let line = parse_positions(v)?;
    if line.len() < 2 {
        return Err(GeoError::InvalidFormat(
            "LineString requires at least two positions".into(),
        ));
    }
    Ok(line)
}

fn parse_ring(v: &Value) -> Result<Vec<Coordinate>, GeoError> {
    let ring = parse_positions(v)?;
    let positions = v.as_array().expect("parse_positions checked the array");
    // Compare every ordinate (including altitude) before the 2D projection.
    let closed = positions
        .first()
        .and_then(Value::as_array)
        .zip(positions.last().and_then(Value::as_array))
        .is_some_and(|(first, last)| {
            first.len() == last.len()
                && first
                    .iter()
                    .zip(last)
                    .all(|(a, b)| a.as_f64() == b.as_f64())
        });
    if ring.len() < 4 || !closed {
        return Err(GeoError::InvalidFormat(
            "Polygon rings require at least four positions and identical endpoints".into(),
        ));
    }
    Ok(ring)
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
