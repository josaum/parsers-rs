//! WFS (Web Feature Service) client — OGC 2.0.0.
//!
//! Enabled via the `http` feature flag.
//!
//! Fetches features from a WFS endpoint, returning them parsed as [`GeoFeature`]s
//! via the GeoJSON output format (`outputFormat=application/json`).

#![cfg(feature = "http")]

use reqwest::Client;
use serde_json::Value;

use crate::error::GeoError;
use crate::geojson::{check_nesting_depth, geojson_to_features};
use crate::http_body::read_body_within;
use crate::types::{BBox, GeoFeature};

/// Largest response [`WfsClient`] reads: 256 MiB.
///
/// [`WfsClient::get_features`] asks for at most 10,000 features. A polygon
/// drawn in detail, such as an administrative boundary, runs to a thousand
/// vertices or more, and GeoJSON writes a vertex in about 25 bytes
/// (`[-46.633308,-23.550520],`): 10,000 such features are about 250 MB.
/// Points and simple lines take a small fraction of that. The same limit
/// applies to [`WfsClient::get_raw`], which asks for every feature the
/// server agrees to send.
///
/// The body is counted as it arrives and the request fails with
/// [`GeoError::Http`] once it would pass this size, before any of it is
/// parsed; a response that announces a longer body in `Content-Length`
/// fails before any of it is read. Parsed into a [`serde_json::Value`], a
/// document takes several times its length in memory, so this is also what
/// bounds that. The GeoJSON readers of this crate have no limit on length,
/// only on nesting, which the WFS client applies too.
pub const MAX_WFS_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// WFS client.
#[derive(Debug, Clone)]
pub struct WfsClient {
    /// Service endpoint, without query parameters.
    pub base_url: String,
    /// Feature type requested in `TYPENAMES`.
    pub type_name: String,
    client: Client,
}

impl WfsClient {
    /// Create a new WFS client.
    ///
    /// `type_name` is the WFS `typeName` (layer identifier) to query.
    pub fn new(base_url: impl Into<String>, type_name: impl Into<String>) -> Self {
        WfsClient {
            base_url: base_url.into(),
            type_name: type_name.into(),
            client: Client::new(),
        }
    }

    /// Build a `GetFeature` request URL with optional BBOX filter.
    ///
    /// Uses WFS 2.0.0 syntax and requests GeoJSON output.
    pub fn get_feature_url(&self, bbox_filter: Option<&BBox>, max_features: Option<u32>) -> String {
        let mut url = format!(
            "{}?SERVICE=WFS&VERSION=2.0.0&REQUEST=GetFeature\
             &TYPENAMES={type_name}\
             &OUTPUTFORMAT=application/json",
            self.base_url,
            type_name = self.type_name,
        );

        if let Some(bbox) = bbox_filter {
            url.push_str(&format!(
                "&BBOX={},{},{},{}",
                bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat
            ));
        }

        if let Some(max) = max_features {
            url.push_str(&format!("&COUNT={max}"));
        }

        url
    }

    /// Fetch features from the WFS endpoint.
    ///
    /// The server must support GeoJSON output format
    /// (`outputFormat=application/json`).
    ///
    /// # Errors
    ///
    /// Returns [`GeoError::Http`] when the request fails, the server answers
    /// with an error status, the response is longer than
    /// [`MAX_WFS_RESPONSE_BYTES`] or is not JSON, and
    /// [`GeoError::InvalidFormat`] when it nests arrays and objects deeper
    /// than [`parse_geojson_str`](crate::geojson::parse_geojson_str) allows
    /// or is not a GeoJSON `FeatureCollection` or `Feature`.
    pub async fn get_features(
        &self,
        bbox_filter: Option<&BBox>,
    ) -> Result<Vec<GeoFeature>, GeoError> {
        let url = self.get_feature_url(bbox_filter, Some(10_000));
        let body = self.get_json(&url, MAX_WFS_RESPONSE_BYTES).await?;
        geojson_to_features(&body)
    }

    /// Fetch raw GeoJSON response as a parsed `serde_json::Value`.
    ///
    /// # Errors
    ///
    /// Returns [`GeoError::Http`] when the request fails, the server answers
    /// with an error status, the response is longer than
    /// [`MAX_WFS_RESPONSE_BYTES`] or is not JSON, and
    /// [`GeoError::InvalidFormat`] when it nests arrays and objects deeper
    /// than [`parse_geojson_str`](crate::geojson::parse_geojson_str) allows.
    pub async fn get_raw(&self, bbox_filter: Option<&BBox>) -> Result<Value, GeoError> {
        let url = self.get_feature_url(bbox_filter, None);
        self.get_json(&url, MAX_WFS_RESPONSE_BYTES).await
    }

    /// Asks `url` for JSON and parses a response of at most `max` bytes.
    async fn get_json(&self, url: &str, max: usize) -> Result<Value, GeoError> {
        let resp = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| GeoError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(GeoError::Http(format!(
                "WFS GetFeature returned HTTP {}",
                resp.status()
            )));
        }

        let body = read_body_within(resp, max, "WFS GetFeature").await?;
        check_nesting_depth(&body)?;
        serde_json::from_slice(&body).map_err(|e| GeoError::Http(format!("JSON decode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_body::test_server::{local_client, ok_response, serve_once};

    /// A client for the server at `base_url` that ignores any proxy.
    fn local_wfs(base_url: String) -> WfsClient {
        WfsClient {
            base_url,
            type_name: "t".into(),
            client: local_client(),
        }
    }

    /// An empty feature collection padded with spaces to `length` bytes.
    fn padded_collection(length: usize) -> Vec<u8> {
        let mut body = br#"{"type":"FeatureCollection","features":[]}"#.to_vec();
        body.resize(length, b' ');
        body
    }

    /// Whether `result` is the refusal of a response over `max`.
    fn refused<T>(result: &Result<T, GeoError>, max: usize) -> bool {
        matches!(
            result,
            Err(GeoError::Http(message))
                if *message == format!("WFS GetFeature response is larger than {max} bytes")
        )
    }

    /// The response used to be parsed whole, whatever its length.
    #[tokio::test]
    async fn response_over_the_limit_is_refused() {
        // Announced: refused on the header, before the body is sent.
        let max = MAX_WFS_RESPONSE_BYTES;
        let announced = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{{}}", max + 1);
        let client = local_wfs(serve_once(announced.into_bytes()));
        assert!(refused(&client.get_raw(None).await, max));
        let announced = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{{}}", max + 1);
        let client = local_wfs(serve_once(announced.into_bytes()));
        assert!(refused(&client.get_features(None).await, max));

        // Not announced, against a smaller limit: refused while reading.
        let client = local_wfs(serve_once(ok_response(&padded_collection(1001), false)));
        let url = client.get_feature_url(None, None);
        assert!(refused(&client.get_json(&url, 1000).await, 1000));

        // Exactly the limit is read and parsed, announced or not.
        for announced in [false, true] {
            let exact = ok_response(&padded_collection(1000), announced);
            let client = local_wfs(serve_once(exact));
            let url = client.get_feature_url(None, None);
            let value = client.get_json(&url, 1000).await.expect("collection");
            assert_eq!(value["type"], "FeatureCollection");
        }
    }

    /// The response is held to the nesting limit of
    /// [`crate::geojson::parse_geojson_str`]: it used to be parsed with
    /// `serde_json`'s own, of 128 levels.
    #[tokio::test]
    async fn response_nested_deeper_than_geojson_allows_is_refused() {
        let properties = format!("{}1{}", "[".repeat(40), "]".repeat(40));
        let body = format!(
            r#"{{"type":"FeatureCollection","features":[{{"type":"Feature",
            "geometry":{{"type":"Point","coordinates":[1,2]}},
            "properties":{{"deep":{properties}}}}}]}}"#
        );
        let client = local_wfs(serve_once(ok_response(body.as_bytes(), true)));
        let result = client.get_features(None).await;
        assert!(
            matches!(
                &result,
                Err(GeoError::InvalidFormat(message))
                    if message == "GeoJSON nests arrays and objects deeper than 32 levels"
            ),
            "{result:?}"
        );
    }
}
