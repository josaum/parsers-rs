use geo_fast_core::{features_to_geojson, geojson_to_features};
use serde_json::{json, Value};

fn feature(geometry: Value) -> Value {
    json!({"type":"Feature", "properties":null, "geometry":geometry})
}

#[test]
fn identifiers_preserve_escaped_strings_and_omit_missing_ids() {
    let mut value = feature(json!({"type":"Point","coordinates":[1,2]}));
    let out = features_to_geojson(&geojson_to_features(&value).unwrap()).unwrap();
    assert!(out["features"][0].get("id").is_none());
    value["id"] = json!("\"quoted\\path\n\"東京");
    let parsed = geojson_to_features(&value).unwrap();
    assert_eq!(parsed[0].id.as_deref(), value["id"].as_str());
    assert_eq!(
        features_to_geojson(&parsed).unwrap()["features"][0]["id"],
        value["id"]
    );
}

#[test]
fn validates_feature_members() {
    let base = feature(json!({"type":"Point","coordinates":[1,2]}));
    for id in [Value::Null, json!(true), json!([]), json!({})] {
        let mut value = base.clone();
        value["id"] = id;
        assert!(geojson_to_features(&value).is_err());
    }
    for properties in [json!(1), json!("x"), json!([])] {
        let mut value = base.clone();
        value["properties"] = properties;
        assert!(geojson_to_features(&value).is_err());
    }
    let mut value = base.clone();
    value.as_object_mut().unwrap().remove("properties");
    assert!(geojson_to_features(&value).is_err());
    value["type"] = json!("Point");
    assert!(geojson_to_features(&json!({"type":"FeatureCollection","features":[value]})).is_err());
}

#[test]
fn validates_line_and_polygon_geometry() {
    for geometry in [
        json!({"type":"Point","coordinates":[1,2,"invalid altitude"]}),
        json!({"type":"LineString","coordinates":[[1,2]]}),
        json!({"type":"MultiLineString","coordinates":[[[1,2]]]}),
        json!({"type":"Polygon","coordinates":[[[0,0],[1,0],[0,0]]]}),
        json!({"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1]]]}),
        json!({"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,1]]]]}),
        json!({"type":"Polygon","coordinates":[[[0,0,0],[1,0,0],[1,1,0],[0,0,1]]]}),
    ] {
        assert!(
            geojson_to_features(&feature(geometry.clone())).is_err(),
            "{geometry}"
        );
    }
    // RFC 7946 recommends accepting either winding direction when reading.
    for ring in [
        json!([[0, 0], [1, 0], [1, 1], [0, 0]]),
        json!([[0, 0], [1, 1], [1, 0], [0, 0]]),
    ] {
        assert!(
            geojson_to_features(&feature(json!({"type":"Polygon","coordinates":[ring]}))).is_ok()
        );
    }
}
