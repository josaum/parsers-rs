//! WMS (Web Map Service) client — OGC 1.1.1 / 1.3.0.
//!
//! Enabled via the `http` feature flag.
//!
//! Capabilities parsing extracts layer names, supported CRS codes, and bounding
//! boxes from the XML `GetCapabilities` response.

#![cfg(feature = "http")]

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use reqwest::Client;

use crate::error::GeoError;
use crate::types::BBox;
use crate::xml_text::TextRun;

/// A layer description extracted from a WMS `GetCapabilities` response.
#[derive(Debug, Clone)]
pub struct WmsLayer {
    /// Layer identifier, the value a `GetMap` request passes in `LAYERS`.
    pub name: String,
    /// Human-readable layer title.
    pub title: String,
    /// Coordinate reference systems the server offers the layer in.
    pub crs_list: Vec<String>,
    /// Geographic extent of the layer, when the server declares one.
    pub bbox: Option<BBox>,
}

/// WMS client.
#[derive(Debug, Clone)]
pub struct WmsClient {
    /// Service endpoint, without query parameters.
    pub base_url: String,
    /// Layer names requested in `LAYERS`.
    pub layers: Vec<String>,
    /// Coordinate reference system requested, such as `EPSG:4326`.
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

/// Reads the named layers out of a WMS `GetCapabilities` document.
///
/// A layer without a `<Name>` is a grouping node and is not returned.
///
/// # Errors
///
/// Returns [`GeoError`] when `xml` is not well-formed.
pub fn parse_capabilities_xml(xml: &str) -> Result<Vec<WmsLayer>, GeoError> {
    let mut reader = Reader::from_str(xml);
    // The reader does not trim: a value arrives in pieces (see `TextRun`),
    // and trimming each piece would close up `a &amp; b` to `a&b`. A `&` that
    // starts no reference is reported as text, not as an error, so one
    // unescaped ampersand in a title does not cost the whole document.
    reader.config_mut().trim_text(false);
    reader.config_mut().allow_dangling_amp = true;
    let mut run = TextRun::default();

    let mut layers: Vec<WmsLayer> = Vec::new();
    let mut current_layer: Option<WmsLayerBuilder> = None;
    let mut inside_layer = false;
    let mut capture_name = false;
    let mut capture_title = false;
    let mut capture_crs = false;
    let mut depth: usize = 0;
    let mut layer_depth: usize = 0;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| GeoError::Xml(e.to_string()))?;
        match &event {
            Event::Text(piece) => {
                run.push_text(piece);
                continue;
            }
            Event::GeneralRef(reference) => {
                run.push_reference(reference);
                continue;
            }
            _ => {}
        }

        // Any other event ends the character data collected so far.
        if let Some(text) = run.take() {
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

        match event {
            Event::Start(ref e) => {
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
            Event::Empty(ref e) => {
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
                                    let val = attr
                                        .normalized_value(XmlVersion::Implicit1_0)
                                        .unwrap_or_default();
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
            Event::End(ref e) => {
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
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
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

    /// The name, title and CRS list of each layer of a document whose first
    /// layer has `value` as its title and CRS, and whose second has it as
    /// its name.
    fn layers_with(value: &str) -> Vec<(String, String, Vec<String>)> {
        let capabilities = format!(
            "<C><Layer><Name>n</Name><Title>{value}</Title><CRS>{value}</CRS></Layer>\
             <Layer><Name>{value}</Name><Title>t</Title></Layer></C>"
        );
        parse_capabilities_xml(&capabilities)
            .expect("capabilities")
            .into_iter()
            .map(|layer| (layer.name, layer.title, layer.crs_list))
            .collect()
    }

    /// Both layers of [`layers_with`], had `value` been read as `read`.
    fn both(read: &str) -> Vec<(String, String, Vec<String>)> {
        vec![
            ("n".into(), read.into(), vec![read.into()]),
            (read.into(), "t".into(), vec![]),
        ]
    }

    /// The values quick-xml 0.36 produced, when a reference was part of the
    /// text event around it. From 0.41 a reference is an event of its own and
    /// a value arrives in pieces; the first piece of `A &amp; B` is `A `.
    #[test]
    fn references_in_a_layer_title_are_resolved_in_place() {
        for (written, read) in [
            ("A &amp; B", "A & B"),
            ("Caf&#233;", "Café"),
            ("Caf&#xE9;", "Café"),
            ("&amp;", "&"),
            ("a&lt;b", "a<b"),
            ("  padded &amp;  ", "padded &"),
            // Whitespace written as a reference is content.
            ("&#32;x&#32;", " x "),
            (" &#32;x&#32; ", " x "),
            ("EPSG&#58;4326", "EPSG:4326"),
        ] {
            assert_eq!(layers_with(written), both(read), "{written}");
        }
    }

    /// A CDATA section has never been read here, and of the character data
    /// on either side of other markup the first stretch is the value.
    #[test]
    fn cdata_in_a_layer_title_is_not_read() {
        let untitled = vec![("n".to_string(), String::new(), vec![])];
        assert_eq!(layers_with("<![CDATA[X &amp; Y]]>"), untitled);
        assert_eq!(layers_with("   "), untitled);
        assert_eq!(layers_with("pre<![CDATA[mid]]>post"), both("pre"));
        assert_eq!(layers_with("a<!-- c -->b"), both("a"));
        assert_eq!(layers_with("<b>x</b>y"), both("x"));
    }

    #[test]
    fn bounding_box_written_with_references_is_read_whole() {
        let layers = parse_capabilities_xml(
            "<C><Layer><Name>n</Name>\
             <BoundingBox minx=\"&#45;74.0\" miny=\"-3&#51;.8\" maxx=\"-28&#46;5\" maxy=\"5.3\"/>\
             </Layer></C>",
        )
        .expect("capabilities");
        assert_eq!(layers[0].bbox, Some(BBox::new(-74.0, -33.8, -28.5, 5.3)));
    }

    /// quick-xml 0.36 failed to unescape such a value and the reader then
    /// blanked it: a layer named `A & B` had no name and was left out.
    #[test]
    fn reference_that_cannot_be_resolved_is_kept_as_written() {
        for written in ["A &unknown; B", "A & B", "A &#0; B", "AT&T"] {
            assert_eq!(layers_with(written), both(written), "{written}");
        }
    }
}
