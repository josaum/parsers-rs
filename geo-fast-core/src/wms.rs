//! WMS (Web Map Service) client — OGC 1.1.1 / 1.3.0.
//!
//! Enabled via the `http` feature flag.
//!
//! Capabilities parsing extracts layer names, supported CRS codes, and bounding
//! boxes from the XML `GetCapabilities` response.

#![cfg(feature = "http")]

use quick_xml::events::{BytesStart, Event};
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
/// Every `<Layer>` with a `<Name>` is returned, in document order, however
/// deeply it is nested. A layer without a name is a grouping node: it is not
/// returned, but what it declares is still inherited by the layers in it.
///
/// As the WMS specification has it, a layer's reference systems are those of
/// its ancestors followed by its own, each once, and a layer that declares
/// no bounding box has its parent's. Name and title are never inherited, and
/// both are read only from the layer's own `<Name>` and `<Title>` children:
/// those of a `<Style>` or an `<Attribution>` are not the layer's.
///
/// [`WmsLayer::bbox`] is the geographic box (`EX_GeographicBoundingBox`,
/// written with child elements in WMS 1.3.0, or `LatLonBoundingBox` in
/// 1.1.1), the layer's own or the nearest ancestor's. A `BoundingBox` in
/// some other reference system is used only where there is no geographic
/// box on the layer or above it.
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

    // One slot per `<Layer>`, taken when it opens and filled when it closes,
    // so that a parent comes before the children that close before it.
    let mut layers: Vec<Option<WmsLayer>> = Vec::new();
    // The layers still open, outermost first.
    let mut open: Vec<OpenLayer> = Vec::new();
    let mut capture = Capture::Nothing;
    let mut depth: usize = 0;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| GeoError::Xml(e.to_string()))?;
        match &event {
            Event::Text(piece) => {
                run.push_text(piece);
                continue;
            }
            Event::CData(section) => {
                run.push_cdata(section);
                continue;
            }
            Event::GeneralRef(reference) => {
                run.push_reference(reference);
                continue;
            }
            _ => {}
        }

        // Any other event ends the character data collected so far.
        if let (Some(text), Some(layer)) = (run.take(), open.last_mut()) {
            match std::mem::take(&mut capture) {
                Capture::Name => layer.name = text,
                Capture::Title => layer.title = text,
                Capture::Crs => layer.add_crs(text),
                Capture::Edge(edge) => layer.edges[edge] = text.trim().parse().ok(),
                Capture::Nothing => {}
            }
        }

        match event {
            Event::Start(ref e) => {
                depth += 1;
                let name = e.local_name();
                if name.as_ref() == b"Layer" {
                    let slot = layers.len();
                    layers.push(None);
                    let layer = OpenLayer::inside(open.last(), depth, slot);
                    open.push(layer);
                    capture = Capture::Nothing;
                } else if let Some(layer) = open.last_mut() {
                    // Only the layer's own children describe it.
                    let own_child = depth == layer.depth + 1;
                    match name.as_ref() {
                        b"Name" if own_child => capture = Capture::Name,
                        b"Title" if own_child => capture = Capture::Title,
                        b"CRS" | b"SRS" if own_child => capture = Capture::Crs,
                        b"EX_GeographicBoundingBox" if own_child => {
                            layer.edges = [None; 4];
                            layer.in_geographic_box = true;
                            layer.read_box(e);
                        }
                        b"LatLonBoundingBox" | b"BoundingBox" if own_child => layer.read_box(e),
                        edge if layer.in_geographic_box && depth == layer.depth + 2 => {
                            capture = match edge {
                                b"westBoundLongitude" => Capture::Edge(WEST),
                                b"southBoundLatitude" => Capture::Edge(SOUTH),
                                b"eastBoundLongitude" => Capture::Edge(EAST),
                                b"northBoundLatitude" => Capture::Edge(NORTH),
                                _ => Capture::Nothing,
                            };
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(ref e) => {
                if let Some(layer) = open.last_mut() {
                    let is_box = matches!(
                        e.local_name().as_ref(),
                        b"EX_GeographicBoundingBox" | b"LatLonBoundingBox" | b"BoundingBox"
                    );
                    // An empty element does not change `depth`.
                    if is_box && depth == layer.depth {
                        layer.read_box(e);
                    }
                }
            }
            Event::End(ref e) => {
                let name = e.local_name();
                if let Some(layer) = open.last_mut() {
                    if name.as_ref() == b"Layer" && depth == layer.depth {
                        if let Some(closed) = open.pop() {
                            let slot = closed.slot;
                            if let (Some(place), Some(layer)) =
                                (layers.get_mut(slot), closed.into_layer())
                            {
                                *place = Some(layer);
                            }
                        }
                    } else if layer.in_geographic_box && depth == layer.depth + 1 {
                        layer.in_geographic_box = false;
                        if let [Some(west), Some(south), Some(east), Some(north)] = layer.edges {
                            layer.geographic = Some(BBox::new(west, south, east, north));
                        }
                    }
                }
                capture = Capture::Nothing;
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(layers.into_iter().flatten().collect())
}

/// Positions of the four edges in [`OpenLayer::edges`].
const WEST: usize = 0;
const SOUTH: usize = 1;
const EAST: usize = 2;
const NORTH: usize = 3;

/// Where the next run of character data belongs.
#[derive(Default)]
enum Capture {
    /// Nowhere.
    #[default]
    Nothing,
    /// The layer's name.
    Name,
    /// The layer's title.
    Title,
    /// One of the layer's reference systems.
    Crs,
    /// An edge of the layer's geographic box, by its position in
    /// [`OpenLayer::edges`].
    Edge(usize),
}

/// A `<Layer>` whose end tag has not been read yet.
struct OpenLayer {
    /// Depth of the `<Layer>` element in the document.
    depth: usize,
    /// Where the layer goes in the list of all layers.
    slot: usize,
    name: String,
    title: String,
    /// Reference systems: the ancestors', then the layer's own.
    crs_list: Vec<String>,
    /// Geographic box, the layer's own or else the one it inherits.
    geographic: Option<BBox>,
    /// Box in another reference system, likewise.
    bounding: Option<BBox>,
    /// West, south, east and north of an `EX_GeographicBoundingBox` whose
    /// children are being read.
    edges: [Option<f64>; 4],
    /// Whether the reader is inside the layer's `EX_GeographicBoundingBox`.
    in_geographic_box: bool,
}

impl OpenLayer {
    /// A layer opening at `depth` inside `parent`, with what it inherits.
    fn inside(parent: Option<&OpenLayer>, depth: usize, slot: usize) -> Self {
        OpenLayer {
            depth,
            slot,
            name: String::new(),
            title: String::new(),
            crs_list: parent.map(|p| p.crs_list.clone()).unwrap_or_default(),
            geographic: parent.and_then(|p| p.geographic),
            bounding: parent.and_then(|p| p.bounding),
            edges: [None; 4],
            in_geographic_box: false,
        }
    }

    /// Adds a reference system unless the layer already has it.
    fn add_crs(&mut self, crs: String) {
        if !self.crs_list.contains(&crs) {
            self.crs_list.push(crs);
        }
    }

    /// Reads a bounding box written as attributes of `element`.
    ///
    /// The box replaces the one inherited or read before it, of its kind:
    /// geographic for every element but `BoundingBox`. An element without
    /// all four edges as numbers changes nothing.
    fn read_box(&mut self, element: &BytesStart<'_>) {
        let mut min_x: Option<f64> = None;
        let mut min_y: Option<f64> = None;
        let mut max_x: Option<f64> = None;
        let mut max_y: Option<f64> = None;
        for attr in element.attributes().with_checks(false).flatten() {
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
        if let (Some(x0), Some(y0), Some(x1), Some(y1)) = (min_x, min_y, max_x, max_y) {
            let bbox = Some(BBox::new(x0, y0, x1, y1));
            if element.local_name().as_ref() == b"BoundingBox" {
                self.bounding = bbox;
            } else {
                self.geographic = bbox;
            }
        }
    }

    /// The layer as it is returned; `None` for a group without a name.
    fn into_layer(self) -> Option<WmsLayer> {
        (!self.name.is_empty()).then(|| WmsLayer {
            name: self.name,
            title: self.title,
            crs_list: self.crs_list,
            bbox: self.geographic.or(self.bounding),
        })
    }
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

    /// A CDATA section used to be skipped: a layer named only in one had an
    /// empty name and was left out, and `pre<![CDATA[mid]]>post` was read as
    /// `pre`. It is text like the rest of the value, taken as written. Of
    /// the character data on either side of a comment or a child element
    /// the first stretch is still the value.
    #[test]
    fn cdata_in_a_layer_title_is_read_as_text() {
        let untitled = vec![("n".to_string(), String::new(), vec![])];
        assert_eq!(layers_with("<![CDATA[X &amp; Y]]>"), both("X &amp; Y"));
        assert_eq!(
            layers_with("<![CDATA[Roads & <Rivers>]]>"),
            both("Roads & <Rivers>")
        );
        assert_eq!(layers_with("<![CDATA[]]>"), untitled);
        assert_eq!(layers_with("   "), untitled);
        assert_eq!(layers_with("pre<![CDATA[mid]]>post"), both("premidpost"));
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

    /// A root layer with two children, as servers publish them: styles with
    /// names of their own, a group without a name, and both ways of writing
    /// a geographic bounding box.
    const NESTED_CAPS: &str = r#"<WMS_Capabilities version="1.3.0"><Capability>
      <Layer>
        <Name>root</Name>
        <Title>Root</Title>
        <CRS>EPSG:4326</CRS>
        <EX_GeographicBoundingBox>
          <westBoundLongitude>-74</westBoundLongitude>
          <eastBoundLongitude>-28.5</eastBoundLongitude>
          <southBoundLatitude>-33.8</southBoundLatitude>
          <northBoundLatitude>5.3</northBoundLatitude>
        </EX_GeographicBoundingBox>
        <Attribution><Title>Agency</Title></Attribution>
        <Style><Name>root_style</Name><Title>Root style</Title></Style>
        <Layer>
          <Name>roads</Name>
          <Title>Roads</Title>
          <CRS>EPSG:3857</CRS>
          <CRS>EPSG:4326</CRS>
          <Style><Name>default</Name><Title>Default</Title></Style>
        </Layer>
        <Layer>
          <Title>Group</Title>
          <CRS>EPSG:31983</CRS>
          <Layer>
            <Name>rivers</Name>
            <Title>Rivers</Title>
            <LatLonBoundingBox minx="-50" miny="-20" maxx="-40" maxy="-10"/>
            <Style><Name>blue</Name></Style>
          </Layer>
        </Layer>
      </Layer>
    </Capability></WMS_Capabilities>"#;

    /// Nested layers collapsed into one: the root, under the name of the
    /// last `Name` in it (a style's) and the title of the last `Title`.
    #[test]
    fn every_named_layer_is_returned_with_what_it_inherits() {
        let layers = parse_capabilities_xml(NESTED_CAPS).expect("capabilities");
        let strings = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let summary: Vec<_> = layers
            .iter()
            .map(|layer| {
                (
                    layer.name.as_str(),
                    layer.title.as_str(),
                    layer.crs_list.clone(),
                    layer.bbox,
                )
            })
            .collect();
        let root_box = Some(BBox::new(-74.0, -33.8, -28.5, 5.3));
        assert_eq!(
            summary,
            vec![
                ("root", "Root", strings(&["EPSG:4326"]), root_box),
                // The parent's reference systems, then its own, once each.
                (
                    "roads",
                    "Roads",
                    strings(&["EPSG:4326", "EPSG:3857"]),
                    root_box
                ),
                // Through a group that has no name and is not returned.
                (
                    "rivers",
                    "Rivers",
                    strings(&["EPSG:4326", "EPSG:31983"]),
                    Some(BBox::new(-50.0, -20.0, -40.0, -10.0))
                ),
            ]
        );
    }

    /// A layer's own geographic box is its extent whatever order its boxes
    /// are written in; a box in a projected system is used when there is no
    /// geographic one, on the layer or above it. A title is not inherited.
    #[test]
    fn geographic_box_is_preferred_and_titles_are_not_inherited() {
        let layers = parse_capabilities_xml(
            "<C><Layer><Name>a</Name><Title>A</Title>\
             <LatLonBoundingBox minx=\"1\" miny=\"2\" maxx=\"3\" maxy=\"4\"/>\
             <BoundingBox SRS=\"EPSG:3857\" minx=\"10\" miny=\"20\" maxx=\"30\" maxy=\"40\"/>\
             <Layer><Name>b</Name>\
             <BoundingBox SRS=\"EPSG:3857\" minx=\"11\" miny=\"21\" maxx=\"31\" maxy=\"41\"/>\
             </Layer></Layer>\
             <Layer><Name>c</Name>\
             <BoundingBox minx=\"5\" miny=\"6\" maxx=\"7\" maxy=\"8\"/></Layer></C>",
        )
        .expect("capabilities");
        let summary: Vec<_> = layers
            .iter()
            .map(|layer| (layer.name.as_str(), layer.title.as_str(), layer.bbox))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("a", "A", Some(BBox::new(1.0, 2.0, 3.0, 4.0))),
                ("b", "", Some(BBox::new(1.0, 2.0, 3.0, 4.0))),
                ("c", "", Some(BBox::new(5.0, 6.0, 7.0, 8.0))),
            ]
        );
    }
}
