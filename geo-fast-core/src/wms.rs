//! WMS (Web Map Service) client — OGC 1.1.1 / 1.3.0.
//!
//! Enabled via the `http` feature flag.
//!
//! Capabilities parsing extracts layer names, supported CRS codes, and bounding
//! boxes from the XML `GetCapabilities` response.

#![cfg(feature = "http")]

use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;

use crate::error::GeoError;
use crate::types::BBox;

/// A layer description extracted from a WMS `GetCapabilities` response.
#[derive(Debug, Clone)]
pub struct WmsLayer {
    pub name: String,
    pub title: String,
    pub crs_list: Vec<String>,
    pub bbox: Option<BBox>,
}

/// WMS client.
#[derive(Debug, Clone)]
pub struct WmsClient {
    pub base_url: String,
    pub layers: Vec<String>,
    pub crs: String,
    client: Client,
}

impl WmsClient {
    /// Create a new WMS client.
    ///
    /// `layers` is the comma-separated LAYERS parameter value.
    /// `crs` defaults to `"EPSG:4326"`.
    pub fn new(base_url: impl Into<String>, layers: Vec<String>, crs: Option<String>) -> Self {
        WmsClient {
            base_url: base_url.into(),
            layers,
            crs: crs.unwrap_or_else(|| "EPSG:4326".into()),
            client: Client::new(),
        }
    }

    /// Build a `GetCapabilities` request URL.
    pub fn capabilities_url(&self) -> String {
        format!(
            "{}?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
            self.base_url
        )
    }

    /// Build a `GetMap` request URL.
    ///
    /// `bbox` must be in the CRS of `self.crs`.
    pub fn get_map_url(
        &self,
        bbox: &BBox,
        width: u32,
        height: u32,
        format: Option<&str>,
    ) -> String {
        let fmt = format.unwrap_or("image/png");
        let layers = self.layers.join(",");
        format!(
            "{}?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap\
             &LAYERS={layers}\
             &STYLES=\
             &CRS={crs}\
             &BBOX={min_x},{min_y},{max_x},{max_y}\
             &WIDTH={width}\
             &HEIGHT={height}\
             &FORMAT={fmt}\
             &TRANSPARENT=TRUE",
            self.base_url,
            layers = layers,
            crs = self.crs,
            min_x = bbox.min_lon,
            min_y = bbox.min_lat,
            max_x = bbox.max_lon,
            max_y = bbox.max_lat,
            width = width,
            height = height,
            fmt = fmt,
        )
    }

    /// Fetch a rendered map tile as raw bytes (PNG by default).
    pub async fn get_map(&self, bbox: &BBox, width: u32, height: u32) -> Result<Vec<u8>, GeoError> {
        let url = self.get_map_url(bbox, width, height, None);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| GeoError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(GeoError::Http(format!(
                "WMS GetMap returned HTTP {}",
                resp.status()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| GeoError::Http(e.to_string()))
    }

    /// Fetch and parse `GetCapabilities`, returning a list of available layers.
    pub async fn get_capabilities(&self) -> Result<Vec<WmsLayer>, GeoError> {
        let url = self.capabilities_url();
        let xml = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| GeoError::Http(e.to_string()))?
            .text()
            .await
            .map_err(|e| GeoError::Http(e.to_string()))?;

        parse_capabilities_xml(&xml)
    }
}

// ── XML capabilities parser ───────────────────────────────────────────────────

pub fn parse_capabilities_xml(xml: &str) -> Result<Vec<WmsLayer>, GeoError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut layers: Vec<WmsLayer> = Vec::new();
    let mut current_layer: Option<WmsLayerBuilder> = None;
    let mut inside_layer = false;
    let mut capture_name = false;
    let mut capture_title = false;
    let mut capture_crs = false;
    let mut depth: usize = 0;
    let mut layer_depth: usize = 0;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                match e.local_name().as_ref() {
                    b"Layer" => {
                        if !inside_layer {
                            inside_layer = true;
                            layer_depth = depth;
                            current_layer = Some(WmsLayerBuilder::default());
                        }
                    }
                    b"Name" if inside_layer => capture_name = true,
                    b"Title" if inside_layer => capture_title = true,
                    b"CRS" | b"SRS" if inside_layer => capture_crs = true,
                    b"EX_GeographicBoundingBox" | b"LatLonBoundingBox" if inside_layer => {
                        // Handled via attributes on empty elements or children
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                if inside_layer {
                    match e.local_name().as_ref() {
                        b"EX_GeographicBoundingBox" | b"LatLonBoundingBox" | b"BoundingBox" => {
                            if let Some(ref mut builder) = current_layer {
                                let mut min_x: Option<f64> = None;
                                let mut min_y: Option<f64> = None;
                                let mut max_x: Option<f64> = None;
                                let mut max_y: Option<f64> = None;
                                for attr in e.attributes().with_checks(false).flatten() {
                                    let key = attr.key.local_name();
                                    let val = attr.unescape_value().unwrap_or_default();
                                    let f = val.parse::<f64>().ok();
                                    match key.as_ref() {
                                        b"minx" | b"westBoundLongitude" => min_x = f,
                                        b"miny" | b"southBoundLatitude" => min_y = f,
                                        b"maxx" | b"eastBoundLongitude" => max_x = f,
                                        b"maxy" | b"northBoundLatitude" => max_y = f,
                                        _ => {}
                                    }
                                }
                                if let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                                    (min_x, min_y, max_x, max_y)
                                {
                                    builder.bbox = Some(BBox::new(x0, y0, x1, y1));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if let Some(ref mut builder) = current_layer {
                    if capture_name {
                        builder.name = text;
                        capture_name = false;
                    } else if capture_title {
                        builder.title = text;
                        capture_title = false;
                    } else if capture_crs {
                        builder.crs_list.push(text);
                        capture_crs = false;
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if inside_layer && e.local_name().as_ref() == b"Layer" && depth == layer_depth {
                    if let Some(builder) = current_layer.take() {
                        if !builder.name.is_empty() {
                            layers.push(WmsLayer {
                                name: builder.name,
                                title: builder.title,
                                crs_list: builder.crs_list,
                                bbox: builder.bbox,
                            });
                        }
                    }
                    inside_layer = false;
                }
                capture_name = false;
                capture_title = false;
                capture_crs = false;
                if depth > 0 {
                    depth -= 1;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(GeoError::Xml(e.to_string()));
            }
            _ => {}
        }
    }

    Ok(layers)
}

#[derive(Default)]
struct WmsLayerBuilder {
    name: String,
    title: String,
    crs_list: Vec<String>,
    bbox: Option<BBox>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CAPS: &str = r#"<?xml version="1.0"?>
    <WMS_Capabilities version="1.3.0">
      <Capability>
        <Layer>
          <Name>root</Name>
          <Title>Root Layer</Title>
          <Layer>
            <Name>carta_basica</Name>
            <Title>Carta Básica</Title>
            <CRS>EPSG:4326</CRS>
            <CRS>EPSG:3857</CRS>
            <BoundingBox minx="-74.0" miny="-33.8" maxx="-28.5" maxy="5.3"/>
          </Layer>
        </Layer>
      </Capability>
    </WMS_Capabilities>"#;

    #[test]
    fn test_parse_capabilities() {
        let layers = parse_capabilities_xml(SAMPLE_CAPS).unwrap();
        // At least the named child layer should be found
        let carta = layers.iter().find(|l| l.name == "carta_basica");
        assert!(
            carta.is_some(),
            "carta_basica layer not found, got: {:?}",
            layers.iter().map(|l| &l.name).collect::<Vec<_>>()
        );
        let l = carta.unwrap();
        assert!(l.crs_list.contains(&"EPSG:4326".to_string()));
    }
}
