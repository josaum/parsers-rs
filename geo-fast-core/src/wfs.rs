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
use crate::geojson::geojson_to_features;
use crate::types::{BBox, GeoFeature};

/// WFS client.
#[derive(Debug, Clone)]
pub struct WfsClient {
    pub base_url: String,
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
    pub async fn get_features(
        &self,
        bbox_filter: Option<&BBox>,
    ) -> Result<Vec<GeoFeature>, GeoError> {
        let url = self.get_feature_url(bbox_filter, Some(10_000));
        let resp = self
            .client
            .get(&url)
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

        let body: Value = resp
            .json()
            .await
            .map_err(|e| GeoError::Http(format!("JSON decode: {e}")))?;

        geojson_to_features(&body)
    }

    /// Fetch raw GeoJSON response as a parsed `serde_json::Value`.
    pub async fn get_raw(&self, bbox_filter: Option<&BBox>) -> Result<Value, GeoError> {
        let url = self.get_feature_url(bbox_filter, None);
        let resp = self
            .client
            .get(&url)
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

        resp.json::<Value>()
            .await
            .map_err(|e| GeoError::Http(format!("JSON decode: {e}")))
    }
}
