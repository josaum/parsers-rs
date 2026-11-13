//! WMS (Web Map Service) client — OGC 1.1.1 / 1.3.0.
//!
//! Enabled via the `http` feature flag.
//!
//! Capabilities parsing extracts layer names, supported CRS codes, and bounding
//! boxes from the XML `GetCapabilities` response.

#![cfg(feature = "http")]

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

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
    ///
    /// # Errors
    ///
    /// Returns [`GeoError::Http`] when the request fails or the response is
    /// longer than [`MAX_CAPABILITIES_RESPONSE_BYTES`], and whatever
    /// [`parse_capabilities_xml`] returns for the document.
    ///
    /// A document whose layers inherit more reference systems than
    /// [`MAX_INHERITED_CRS_BYTES`] allows is not an error: its later layers
    /// come back without the reference systems of their ancestors, and
    /// nothing in the result says which. See [`parse_capabilities_xml`].
    pub async fn get_capabilities(&self) -> Result<Vec<WmsLayer>, GeoError> {
        let url = self.capabilities_url();
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| GeoError::Http(e.to_string()))?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_body_within(response, MAX_CAPABILITIES_RESPONSE_BYTES).await?;
        let xml = decode_body(content_type.as_deref(), &body);
        drop(body);

        parse_capabilities_xml(&xml)
    }
}

/// Largest `GetCapabilities` response [`WmsClient::get_capabilities`]
/// reads: 128 MiB.
///
/// The server decides how long its response is, so the body is counted as
/// it arrives and the request fails with [`GeoError::Http`] once it would
/// pass this size; a response that announces a longer body in
/// `Content-Length` fails before any of it is read. The largest
/// capabilities documents public servers publish are tens of megabytes
/// (catalogues with tens of thousands of layers); the limit is a few times
/// that, so none of them is refused. While the body is decoded it is held
/// twice, once as bytes and once as text.
pub const MAX_CAPABILITIES_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

/// Reads the body of `response`, refusing one longer than `max` bytes.
///
/// The length is checked before each piece is kept, so no more than `max`
/// bytes and the piece in hand are ever held.
async fn read_body_within(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, GeoError> {
    let too_large = || {
        GeoError::Http(format!(
            "WMS GetCapabilities response is larger than {max} bytes"
        ))
    };
    let announced = response.content_length();
    if announced.is_some_and(|length| usize::try_from(length).map_or(true, |length| length > max)) {
        return Err(too_large());
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| GeoError::Http(e.to_string()))?
    {
        if piece.len() > max.saturating_sub(body.len()) {
            return Err(too_large());
        }
        body.extend_from_slice(&piece);
    }
    Ok(body)
}

/// Decodes a response body as [`reqwest::Response::text`] does.
///
/// The character set is the `charset` parameter of `content_type`, UTF-8
/// when there is none or it names no known encoding. A byte order mark
/// overrides it, and bytes that are not valid become U+FFFD.
fn decode_body(content_type: Option<&str>, body: &[u8]) -> String {
    let label = content_type.and_then(|value| {
        value.split(';').skip(1).find_map(|parameter| {
            let (key, charset) = parameter.split_once('=')?;
            key.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| charset.trim().trim_matches('"'))
        })
    });
    let encoding = label
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
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
/// no bounding box has its parent's. A layer inherits everything its
/// ancestors declare, wherever in them it is written: before the layer or
/// after it. Name and title are never inherited, and
/// both are read only from the layer's own `<Name>` and `<Title>` children:
/// those of a `<Style>` or an `<Attribution>` are not the layer's.
///
/// [`WmsLayer::bbox`] is the geographic box (`EX_GeographicBoundingBox`,
/// written with child elements in WMS 1.3.0, or `LatLonBoundingBox` in
/// 1.1.1), the layer's own or the nearest ancestor's. A `BoundingBox` in
/// some other reference system is used only where there is no geographic
/// box on the layer or above it.
///
/// Each layer is given its own copy of the reference systems it inherits,
/// so a small document with many layers under a long list would grow into
/// a very large result. The copies are limited to
/// [`MAX_INHERITED_CRS_BYTES`] for the whole document, and inheritance of
/// reference systems stops, silently, where that is spent. A layer whose
/// copy no longer fits is still returned, with its name, its title, the
/// reference systems it declares itself and the bounding box it inherits,
/// but without the reference systems of its ancestors; the layers in it
/// inherit that shortened list. The result does not say which layers were
/// cut short, so a caller cannot detect it today: a [`WmsLayer::crs_list`]
/// shorter than that of the layers before it under the same parent is the
/// only sign.
///
/// # Errors
///
/// Returns [`GeoError::Xml`] when `xml` is not well-formed.
pub fn parse_capabilities_xml(xml: &str) -> Result<Vec<WmsLayer>, GeoError> {
    let mut reader = Reader::from_str(xml);
    // The reader does not trim: a value arrives in pieces (see `TextRun`),
    // and trimming each piece would close up `a &amp; b` to `a&b`. A `&` that
    // starts no reference is reported as text, not as an error, so one
    // unescaped ampersand in a title does not cost the whole document.
    reader.config_mut().trim_text(false);
    reader.config_mut().allow_dangling_amp = true;
    let mut run = TextRun::default();

    // One record per `<Layer>`, in the order they open, so that a parent
    // comes before the children that close before it.
    let mut layers: Vec<LayerRecord> = Vec::new();
    // The layers still open, outermost first.
    let mut open: Vec<OpenLayer> = Vec::new();
    // The `EX_GeographicBoundingBox` elements still open, likewise. Kept
    // apart from the layers: a layer is in one only for a moment.
    let mut boxes: Vec<OpenBox> = Vec::new();
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
        if let (Some(text), Some(layer)) = (run.take(), open.last()) {
            match std::mem::take(&mut capture) {
                Capture::Name => declare(&mut layers, layer.record, |own| own.name = text),
                Capture::Title => declare(&mut layers, layer.record, |own| own.title = text),
                // Repeats are dropped once, when the document has been read.
                Capture::Crs => declare(&mut layers, layer.record, |own| own.crs_list.push(text)),
                Capture::Edge(edge) => {
                    let place = boxes
                        .last_mut()
                        .filter(|open_box| open_box.layer_depth == layer.depth)
                        .and_then(|open_box| open_box.edges.get_mut(edge));
                    if let Some(place) = place {
                        *place = text.trim().parse().ok();
                    }
                }
                Capture::Nothing => {}
            }
        }

        match event {
            Event::Start(ref e) => {
                depth += 1;
                let name = e.local_name();
                if name.as_ref() == b"Layer" {
                    let record = layers.len();
                    layers.push(LayerRecord {
                        parent: open.last().map(|parent| parent.record),
                        declared: None,
                    });
                    open.push(OpenLayer { depth, record });
                    capture = Capture::Nothing;
                } else if let Some(layer) = open.last() {
                    // Only the layer's own children describe it.
                    let own_child = depth == layer.depth + 1;
                    let in_geographic_box = boxes
                        .last()
                        .is_some_and(|open_box| open_box.layer_depth == layer.depth);
                    match name.as_ref() {
                        b"Name" if own_child => capture = Capture::Name,
                        b"Title" if own_child => capture = Capture::Title,
                        b"CRS" | b"SRS" if own_child => capture = Capture::Crs,
                        b"EX_GeographicBoundingBox" if own_child => {
                            boxes.push(OpenBox {
                                layer_depth: layer.depth,
                                edges: [None; 4],
                            });
                            read_box(&mut layers, layer.record, e);
                        }
                        b"LatLonBoundingBox" | b"BoundingBox" if own_child => {
                            read_box(&mut layers, layer.record, e);
                        }
                        edge if in_geographic_box && depth == layer.depth + 2 => {
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
                if let Some(layer) = open.last() {
                    let is_box = matches!(
                        e.local_name().as_ref(),
                        b"EX_GeographicBoundingBox" | b"LatLonBoundingBox" | b"BoundingBox"
                    );
                    // An empty element does not change `depth`.
                    if is_box && depth == layer.depth {
                        read_box(&mut layers, layer.record, e);
                    }
                }
            }
            Event::End(ref e) => {
                let name = e.local_name();
                if let Some(layer) = open.last() {
                    let in_geographic_box = boxes
                        .last()
                        .is_some_and(|open_box| open_box.layer_depth == layer.depth);
                    if name.as_ref() == b"Layer" && depth == layer.depth {
                        open.pop();
                    } else if in_geographic_box && depth == layer.depth + 1 {
                        let edges = boxes.pop().map(|open_box| open_box.edges);
                        if let Some([Some(west), Some(south), Some(east), Some(north)]) = edges {
                            declare(&mut layers, layer.record, |own| {
                                own.geographic = Some(BBox::new(west, south, east, north));
                            });
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

    // A layer the document ends inside of is not returned, but the layers
    // that closed in it still inherit what it declared.
    for unclosed in open {
        let declared = layers
            .get_mut(unclosed.record)
            .and_then(|record| record.declared.as_deref_mut());
        if let Some(declared) = declared {
            declared.name.clear();
        }
    }

    Ok(resolve_inheritance(layers))
}

/// Most bytes [`parse_capabilities_xml`] spends on the copies of inherited
/// reference systems, over all the layers of one document: 64 MiB.
///
/// Every copy counts as the length of its text plus
/// [`std::mem::size_of::<String>()`](std::mem::size_of). A reference system
/// a layer declares itself is not counted; the document already holds it.
///
/// Layers are taken in document order. One whose inherited list does not
/// fit in what is left of the budget is not given it, and nothing is
/// counted for it: the layer keeps the reference systems it declares
/// itself and still inherits its bounding box, and the layers in it
/// inherit that shortened list. A later layer with a shorter list to
/// inherit may still be given it. This is silent: the document is not
/// refused, and nothing in a [`WmsLayer`] says its list was cut short, so
/// a caller cannot detect it today.
///
/// As a measure, under a root layer that lists 6000 `EPSG:` codes the
/// first 340 layers are given the list and the ones after them are not.
/// The value is what a result may reasonably hold in copies: enough for
/// every published server whose root lists a few hundred systems, and a
/// bound on what a 300 KB document can be made to allocate.
pub const MAX_INHERITED_CRS_BYTES: usize = 64 * 1024 * 1024;

/// Gives every layer what its ancestors declare and returns the named ones.
///
/// `layers` holds each `<Layer>` in the order they opened, so a parent is
/// always before its children and one pass in that order is enough. It is
/// done once the whole document is read because a parent may declare a
/// reference system or a box after a child has closed.
///
/// The records are resolved where they are: a second list of them would
/// double what a document of many small layers costs.
fn resolve_inheritance(mut layers: Vec<LayerRecord>) -> Vec<WmsLayer> {
    let mut budget = MAX_INHERITED_CRS_BYTES;
    for at in 0..layers.len() {
        let (before, rest) = layers.split_at_mut(at);
        let Some(layer) = rest.first_mut() else {
            break;
        };
        // The nearest layer above this one that declares anything. A layer
        // that declares nothing has already been pointed past itself, so
        // this is one step however long the chain of such layers is.
        layer.parent = layer.parent.and_then(|parent| {
            let above = before.get(parent)?;
            if above.declared.is_some() {
                Some(parent)
            } else {
                above.parent
            }
        });
        let Some(declared) = layer.declared.as_deref_mut() else {
            continue;
        };
        let parent = layer
            .parent
            .and_then(|parent| before.get(parent))
            .and_then(|above| above.declared.as_deref());
        let own = std::mem::take(&mut declared.crs_list);
        if let Some(parent) = parent {
            // Charged before the copy is made. A list that does not fit is
            // not copied, and not looked at either.
            let inherited: &[String] = match budget.checked_sub(parent.crs_bytes) {
                Some(left) => {
                    budget = left;
                    &parent.crs_list
                }
                None => &[],
            };
            declared.crs_list = merge_crs(inherited, own);
            declared.geographic = declared.geographic.or(parent.geographic);
            declared.bounding = declared.bounding.or(parent.bounding);
        } else {
            declared.crs_list = merge_crs(&[], own);
        }
        declared.crs_bytes = copy_cost(&declared.crs_list);
    }
    // Counted first: a list grown as it is filled is for a while held twice.
    let named = layers
        .iter()
        .filter_map(|record| record.declared.as_deref())
        .filter(|declared| !declared.name.is_empty())
        .count();
    let mut returned = Vec::with_capacity(named);
    returned.extend(
        layers
            .into_iter()
            .filter_map(|record| record.declared)
            .filter_map(|declared| declared.into_layer()),
    );
    returned
}

#[cfg(test)]
thread_local! {
    /// How many times two reference systems were compared on this thread.
    static CRS_COMPARISONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A reference system as a member of the set of those a layer has.
struct Seen<'a>(&'a str);

impl PartialEq for Seen<'_> {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(test)]
        CRS_COMPARISONS.with(|count| count.set(count.get() + 1));
        self.0 == other.0
    }
}

impl Eq for Seen<'_> {}

impl Hash for Seen<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// The reference systems of a layer: `inherited`, then those of `own` that
/// are not there yet, each once and in the order they were first written.
///
/// `inherited` is a list this function returned, so it has no repeats. The
/// work is linear in the two lists together: membership is asked of a hash
/// set, not of the list. Nothing is hashed for a layer that declares none.
fn merge_crs(inherited: &[String], own: Vec<String>) -> Vec<String> {
    if own.is_empty() {
        return inherited.to_vec();
    }
    if inherited.is_empty() && own.len() == 1 {
        return own;
    }
    let mut seen: HashSet<Seen<'_>> = HashSet::with_capacity(inherited.len() + own.len());
    seen.extend(inherited.iter().map(|crs| Seen(crs)));
    let is_new: Vec<bool> = own.iter().map(|crs| seen.insert(Seen(crs))).collect();
    drop(seen);
    let added = is_new.iter().filter(|new| **new).count();
    let mut merged = Vec::with_capacity(inherited.len() + added);
    merged.extend_from_slice(inherited);
    merged.extend(
        own.into_iter()
            .zip(is_new)
            .filter_map(|(crs, new)| new.then_some(crs)),
    );
    merged
}

/// What a copy of `crs_list` counts for against [`MAX_INHERITED_CRS_BYTES`].
fn copy_cost(crs_list: &[String]) -> usize {
    crs_list.iter().fold(0usize, |cost, crs| {
        cost.saturating_add(crs.len())
            .saturating_add(std::mem::size_of::<String>())
    })
}

/// Positions of the four edges in [`OpenBox::edges`].
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
    /// [`OpenBox::edges`].
    Edge(usize),
}

/// A `<Layer>` of the document, one per element.
///
/// Kept to two words for a layer that declares nothing: a document can be
/// little more than `<Layer></Layer>` repeated, 15 bytes each.
struct LayerRecord {
    /// The record of the layer this one is in; once resolved, of the
    /// nearest layer above it that declares anything.
    parent: Option<usize>,
    /// What the layer declares, from the first thing it declares.
    declared: Option<Box<Declared>>,
}

/// What a `<Layer>` declares: what it inherits is added by
/// [`resolve_inheritance`].
#[derive(Default)]
struct Declared {
    name: String,
    title: String,
    /// Reference systems: the layer's own as they are written, and once
    /// resolved the ancestors' before them, each once.
    crs_list: Vec<String>,
    /// What a copy of `crs_list` counts for against
    /// [`MAX_INHERITED_CRS_BYTES`].
    crs_bytes: usize,
    /// Geographic box: the layer's own, or once resolved the one it
    /// inherits.
    geographic: Option<BBox>,
    /// Box in another reference system, likewise.
    bounding: Option<BBox>,
}

impl Declared {
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

/// A `<Layer>` the reader is inside of.
#[derive(Clone, Copy)]
struct OpenLayer {
    /// Depth of the `<Layer>` element in the document.
    depth: usize,
    /// Where the layer is in the list of all layers.
    record: usize,
}

/// An `EX_GeographicBoundingBox` whose children are being read.
struct OpenBox {
    /// Depth of the `<Layer>` the box is a child of.
    layer_depth: usize,
    /// West, south, east and north, as far as they have been read.
    edges: [Option<f64>; 4],
}

/// Adds to what the layer at `record` declares, making room for it if this
/// is the first thing the layer declares.
///
/// `record` is always one the reader has pushed; for any other nothing
/// is done.
fn declare(layers: &mut [LayerRecord], record: usize, add: impl FnOnce(&mut Declared)) {
    if let Some(layer) = layers.get_mut(record) {
        add(layer.declared.get_or_insert_with(Box::default));
    }
}

/// Reads a bounding box written as attributes of `element` into the layer
/// at `record`.
///
/// The box replaces the one read before it, of its kind:
/// geographic for every element but `BoundingBox`. An element without
/// all four edges as numbers changes nothing.
fn read_box(layers: &mut [LayerRecord], record: usize, element: &BytesStart<'_>) {
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
        let projected = element.local_name().as_ref() == b"BoundingBox";
        declare(layers, record, |own| {
            if projected {
                own.bounding = bbox;
            } else {
                own.geographic = bbox;
            }
        });
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

    /// 1,500 sibling layers, none inside another: names, titles, repeated
    /// reference systems, every way of writing a box, and groups without a
    /// name.
    fn flat_capabilities() -> String {
        let mut xml = String::from("<C>");
        for i in 0..1500 {
            xml.push_str("<Layer>");
            if i % 7 != 3 {
                xml.push_str(&format!("<Name>layer{i}</Name>"));
            }
            if i % 2 == 0 {
                xml.push_str(&format!("<Title>Layer &amp; {i}</Title>"));
            }
            for system in 0..i % 5 {
                xml.push_str(&format!("<CRS>EPSG:{}</CRS>", 4326 + system));
            }
            if i % 3 == 0 {
                // Declared twice: returned once.
                xml.push_str("<SRS>EPSG:4326</SRS><CRS>CRS:84</CRS>");
            }
            match i % 4 {
                0 => xml.push_str(&format!(
                    "<LatLonBoundingBox minx=\"-{i}\" miny=\"-1\" maxx=\"{i}\" maxy=\"1\"/>"
                )),
                1 => xml.push_str(&format!(
                    "<BoundingBox CRS=\"EPSG:3857\" minx=\"0\" miny=\"0\" maxx=\"{i}\" maxy=\"2\"/>"
                )),
                2 => xml.push_str(&format!(
                    "<EX_GeographicBoundingBox>\
                     <westBoundLongitude>-10</westBoundLongitude>\
                     <eastBoundLongitude>10</eastBoundLongitude>\
                     <southBoundLatitude>-{i}.5</southBoundLatitude>\
                     <northBoundLatitude>{i}.5</northBoundLatitude>\
                     </EX_GeographicBoundingBox>"
                )),
                _ => {}
            }
            xml.push_str("<Style><Name>s</Name><Title>S</Title></Style></Layer>");
        }
        xml.push_str("</C>");
        xml
    }

    /// A document without nesting reads as it did before layers inherited:
    /// the numbers below were taken from the parser as it was.
    #[test]
    fn flat_document_reads_as_before() {
        let layers = parse_capabilities_xml(&flat_capabilities()).expect("capabilities");
        let strings = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let systems: usize = layers.iter().map(|layer| layer.crs_list.len()).sum();
        let boxes = layers.iter().filter(|layer| layer.bbox.is_some()).count();
        let titled = layers
            .iter()
            .filter(|layer| !layer.title.is_empty())
            .count();
        assert_eq!(
            (layers.len(), systems, boxes, titled),
            (1286, 3085, 965, 643)
        );
        let spot = |name: &str| {
            let layer = layers.iter().find(|layer| layer.name == name);
            layer.map(|l| (l.title.clone(), l.crs_list.clone(), l.bbox))
        };
        assert_eq!(
            spot("layer9"),
            Some((
                String::new(),
                strings(&["EPSG:4326", "EPSG:4327", "EPSG:4328", "EPSG:4329", "CRS:84"]),
                Some(BBox::new(0.0, 0.0, 9.0, 2.0))
            ))
        );
        assert_eq!(
            spot("layer750"),
            Some((
                "Layer & 750".to_string(),
                strings(&["EPSG:4326", "CRS:84"]),
                Some(BBox::new(-10.0, -750.5, 10.0, 750.5))
            ))
        );
        assert_eq!(
            spot("layer1499"),
            Some((
                String::new(),
                strings(&["EPSG:4326", "EPSG:4327", "EPSG:4328", "EPSG:4329"]),
                None
            ))
        );
        assert_eq!(layers[0].name, "layer0");
        assert_eq!(layers[1285].name, "layer1499");
    }

    /// Reference systems are told apart through a hash set, so reading
    /// `n` of them takes about `n` comparisons. Each used to be compared
    /// with every one before it: 20,000 took 200,000,000 comparisons, here
    /// and again for every layer that declared what its parent had.
    #[test]
    fn reference_systems_are_deduplicated_in_linear_work() {
        // One layer, 20,000 different systems and each of them once more.
        let mut xml = String::from("<C><Layer><Name>n</Name>");
        for system in (0..20_000).chain(0..20_000) {
            xml.push_str(&format!("<CRS>EPSG:{system}</CRS>"));
        }
        xml.push_str("</Layer></C>");
        CRS_COMPARISONS.with(|count| count.set(0));
        let layers = parse_capabilities_xml(&xml).expect("capabilities");
        let comparisons = CRS_COMPARISONS.with(std::cell::Cell::get);
        assert_eq!(layers[0].crs_list.len(), 20_000);
        assert_eq!(layers[0].crs_list[19_999], "EPSG:19999");
        assert!(comparisons <= 4 * 40_000, "{comparisons} comparisons");

        // A root with 2,000 systems and 50 children that declare them all
        // again, last first, and one of their own.
        let mut xml = String::from("<C><Layer><Name>root</Name>");
        for system in 0..2000 {
            xml.push_str(&format!("<CRS>EPSG:{system}</CRS>"));
        }
        for child in 0..50 {
            xml.push_str(&format!(
                "<Layer><Name>l{child}</Name><CRS>own{child}</CRS>"
            ));
            for system in (0..2000).rev() {
                xml.push_str(&format!("<CRS>EPSG:{system}</CRS>"));
            }
            xml.push_str("</Layer>");
        }
        xml.push_str("</Layer></C>");
        CRS_COMPARISONS.with(|count| count.set(0));
        let layers = parse_capabilities_xml(&xml).expect("capabilities");
        let comparisons = CRS_COMPARISONS.with(std::cell::Cell::get);
        assert_eq!(layers.len(), 51);
        assert!(layers[1..].iter().all(|layer| layer.crs_list.len() == 2001));
        assert_eq!(layers[50].crs_list[1999], "EPSG:1999");
        assert_eq!(layers[50].crs_list[2000], "own49");
        // 102,050 systems are written in the document.
        assert!(comparisons <= 4 * 102_050, "{comparisons} comparisons");
    }

    /// `children` named layers under a root without a name that declares
    /// `systems` reference systems: the shape of a GeoServer root layer.
    fn wide_capabilities(systems: usize, children: usize) -> String {
        let mut xml = String::from(
            "<C><Layer><Title>root</Title>\
             <LatLonBoundingBox minx=\"-74\" miny=\"-34\" maxx=\"-28\" maxy=\"6\"/>",
        );
        for system in 0..systems {
            xml.push_str(&format!("<CRS>EPSG:{system}</CRS>"));
        }
        for child in 0..children {
            xml.push_str(&format!("<Layer><Name>l{child}</Name></Layer>"));
        }
        xml.push_str("</Layer></C>");
        xml
    }

    /// Every layer has a copy of what it inherits, so 265 KB of XML used to
    /// come back as 18,000,000 strings in over 700 MB. One copy of 6,000
    /// `EPSG:` codes counts for 196,890 bytes, and 340 of them fit in
    /// [`MAX_INHERITED_CRS_BYTES`]: the layers after those are returned
    /// without the list, not refused with all the others.
    #[test]
    fn layers_past_the_budget_are_returned_without_inherited_reference_systems() {
        let root_box = Some(BBox::new(-74.0, -34.0, -28.0, 6.0));
        for children in [341, 3000] {
            let xml = wide_capabilities(6000, children);
            assert!(xml.len() < 300_000);
            let layers = parse_capabilities_xml(&xml).expect("capabilities");
            assert_eq!(layers.len(), children);
            for (at, layer) in layers.iter().enumerate() {
                assert_eq!(layer.name, format!("l{at}"));
                assert_eq!(layer.bbox, root_box, "{at}");
                let inherited = if at < 340 { 6000 } else { 0 };
                assert_eq!(layer.crs_list.len(), inherited, "{at}");
            }
            assert_eq!(layers[339].crs_list[5999], "EPSG:5999");
        }
    }

    /// The same amplification with few entries: one long name and many
    /// layers. A layer that is cut keeps what it declares itself, each
    /// once, and the layers in it inherit that.
    #[test]
    fn layer_past_the_budget_keeps_its_own_reference_systems() {
        let long = "x".repeat(100_000);
        let mut xml = format!(
            "<C><Layer><CRS>{long}</CRS>\
             <BoundingBox minx=\"1\" miny=\"2\" maxx=\"3\" maxy=\"4\"/>"
        );
        xml.push_str(&"<Layer><Name>n</Name><CRS>a</CRS><CRS>a</CRS></Layer>".repeat(1999));
        xml.push_str(
            "<Layer><Name>last</Name><CRS>a</CRS><CRS>a</CRS>\
             <Layer><Name>inner</Name><CRS>b</CRS></Layer></Layer></Layer></C>",
        );
        let layers = parse_capabilities_xml(&xml).expect("capabilities");
        assert_eq!(layers.len(), 2001);
        // A copy counts for 100,024 bytes: 670 fit.
        for (at, layer) in layers.iter().enumerate().take(2000) {
            assert_eq!(layer.bbox, Some(BBox::new(1.0, 2.0, 3.0, 4.0)), "{at}");
            if at < 670 {
                assert_eq!(layer.crs_list, [long.as_str(), "a"], "{at}");
            } else {
                assert_eq!(layer.crs_list, ["a"], "{at}");
            }
        }
        // The short list of a cut layer still fits in what is left.
        assert_eq!(layers[2000].name, "inner");
        assert_eq!(layers[2000].crs_list, ["a", "b"]);
        assert_eq!(layers[2000].bbox, Some(BBox::new(1.0, 2.0, 3.0, 4.0)));
    }

    /// Layers nested 3000 deep, one reference system each: the lists grow
    /// until one does not fit, and start again from the layer that was cut.
    #[test]
    fn nested_layers_past_the_budget_inherit_the_shortened_list() {
        let mut xml = String::from("<C>");
        for level in 0..3000 {
            xml.push_str(&format!("<Layer><Name>n{level}</Name><CRS>c{level}</CRS>"));
        }
        xml.push_str(&"</Layer>".repeat(3000));
        xml.push_str("</C>");
        let layers = parse_capabilities_xml(&xml).expect("capabilities");
        assert_eq!(layers.len(), 3000);
        for (level, layer) in layers.iter().enumerate() {
            assert_eq!(layer.name, format!("n{level}"));
            assert_eq!(layer.crs_list.last(), Some(&format!("c{level}")));
        }
        let cut = layers
            .iter()
            .position(|layer| layer.crs_list.len() == 1 && layer.name != "n0")
            .expect("a layer that was cut");
        assert_eq!(layers[cut - 1].crs_list.len(), cut);
        assert_eq!(
            layers[cut + 1].crs_list,
            [format!("c{cut}"), format!("c{}", cut + 1)]
        );
    }

    #[test]
    fn inherited_reference_systems_within_the_budget_are_returned() {
        let layers = parse_capabilities_xml(&wide_capabilities(6000, 200)).expect("capabilities");
        assert_eq!(layers.len(), 200);
        assert!(layers.iter().all(|layer| layer.crs_list.len() == 6000));
    }

    /// Inheritance used to be taken when the child opened, so what a parent
    /// declared after a child was not inherited by it.
    #[test]
    fn what_a_parent_declares_after_a_child_is_inherited() {
        let layers = parse_capabilities_xml(
            "<C><Layer><Name>root</Name><CRS>EPSG:4326</CRS>\
             <LatLonBoundingBox minx=\"-180\" miny=\"-90\" maxx=\"180\" maxy=\"90\"/>\
             <Layer><Name>mid2</Name>\
             <Layer><Name>late</Name><CRS>EPSG:1</CRS></Layer>\
             <CRS>EPSG:9999</CRS>\
             <EX_GeographicBoundingBox>\
             <westBoundLongitude>10</westBoundLongitude>\
             <eastBoundLongitude>20</eastBoundLongitude>\
             <southBoundLatitude>30</southBoundLatitude>\
             <northBoundLatitude>40</northBoundLatitude>\
             </EX_GeographicBoundingBox>\
             </Layer></Layer></C>",
        )
        .expect("capabilities");
        let strings = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let summary: Vec<_> = layers
            .iter()
            .map(|layer| (layer.name.as_str(), layer.crs_list.clone(), layer.bbox))
            .collect();
        let mid_box = Some(BBox::new(10.0, 30.0, 20.0, 40.0));
        assert_eq!(
            summary,
            vec![
                (
                    "root",
                    strings(&["EPSG:4326"]),
                    Some(BBox::new(-180.0, -90.0, 180.0, 90.0))
                ),
                ("mid2", strings(&["EPSG:4326", "EPSG:9999"]), mid_box),
                (
                    "late",
                    strings(&["EPSG:4326", "EPSG:9999", "EPSG:1"]),
                    mid_box
                ),
            ]
        );
    }

    /// A layer that declares nothing costs its record and no more: a list
    /// of 260-byte records, held twice while it was resolved, made 525 MB
    /// of 1,000,000 empty layers in 15 MB.
    #[test]
    fn layer_that_declares_nothing_costs_three_words() {
        assert!(std::mem::size_of::<LayerRecord>() <= 3 * std::mem::size_of::<usize>());
        let layers = parse_capabilities_xml(&"<Layer></Layer>".repeat(10_000)).expect("layers");
        assert!(layers.is_empty());
    }

    /// Layers come back in the order they open, wherever a layer writes
    /// its name: here the outer one after the inner one has closed.
    #[test]
    fn layers_are_returned_in_the_order_they_open() {
        let layers = parse_capabilities_xml(
            "<C><Layer><Layer><Name>b</Name></Layer><Layer><Layer><Name>c</Name></Layer></Layer>\
             <Name>a</Name><CRS>X</CRS></Layer></C>",
        )
        .expect("capabilities");
        let summary: Vec<_> = layers
            .iter()
            .map(|layer| (layer.name.as_str(), layer.crs_list.clone()))
            .collect();
        let x = vec!["X".to_string()];
        assert_eq!(summary, vec![("a", x.clone()), ("b", x.clone()), ("c", x)]);
    }

    /// A document cut short: a layer that closed inside one that did not is
    /// still returned, with what the open one had declared.
    #[test]
    fn layer_inside_one_that_never_closes_is_returned() {
        let layers = parse_capabilities_xml(
            "<C><Layer><Name>open</Name><CRS>A</CRS><Layer><Name>x</Name></Layer>",
        )
        .expect("capabilities");
        let summary: Vec<_> = layers
            .iter()
            .map(|layer| (layer.name.as_str(), layer.crs_list.clone()))
            .collect();
        assert_eq!(summary, vec![("x", vec!["A".to_string()])]);
    }

    /// Serves `response` to the first connection on a local port and
    /// returns the address to ask.
    fn serve_once(response: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                // The client hangs up on a body it refuses.
                let _ = stream.write_all(&response);
            }
        });
        format!("http://{address}")
    }

    fn local_client() -> Client {
        Client::builder().no_proxy().build().expect("client")
    }

    async fn read_within(response: Vec<u8>, max: usize) -> Result<Vec<u8>, GeoError> {
        let url = serve_once(response);
        let response = local_client().get(&url).send().await.expect("response");
        read_body_within(response, max).await
    }

    /// The body used to be read whole, whatever its length.
    #[tokio::test]
    async fn response_body_over_the_cap_is_refused() {
        let refused = |result: Result<Vec<u8>, GeoError>| match result {
            Err(GeoError::Http(message)) => message.contains("larger than 1000 bytes"),
            _ => false,
        };
        // Announced: refused on the header.
        let mut announced = b"HTTP/1.1 200 OK\r\nContent-Length: 1001\r\n\r\n".to_vec();
        announced.extend_from_slice(&[b'x'; 1001]);
        assert!(refused(read_within(announced, 1000).await));

        // Not announced, 4 MB sent until the connection closes: refused
        // while reading.
        let mut unannounced = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        unannounced.extend_from_slice(&vec![b'x'; 4 << 20]);
        assert!(refused(read_within(unannounced, 1000).await));

        // Exactly the cap is read.
        let mut exact = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        exact.extend_from_slice(&[b'x'; 1000]);
        let body = read_within(exact, 1000).await.expect("body");
        assert_eq!(body, vec![b'x'; 1000]);
    }

    /// The response is still decoded in the character set it names.
    #[tokio::test]
    async fn capabilities_are_decoded_in_the_declared_character_set() {
        assert_eq!(
            decode_body(Some("text/xml; charset=ISO-8859-1"), b"Caf\xe9"),
            "Café"
        );
        assert_eq!(
            decode_body(Some("text/xml;Charset=\"utf-8\""), "Café".as_bytes()),
            "Café"
        );
        assert_eq!(
            decode_body(Some("text/xml"), b"Caf\xc3\xa9 \xff"),
            "Café \u{fffd}"
        );
        assert_eq!(decode_body(None, b"\xef\xbb\xbfa"), "a");

        let body = b"<C><Layer><Name>n</Name><Title>Caf\xe9</Title></Layer></C>";
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=iso-8859-1\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let client = WmsClient {
            base_url: serve_once(response),
            layers: Vec::new(),
            crs: "EPSG:4326".into(),
            client: local_client(),
        };
        let layers = client.get_capabilities().await.expect("capabilities");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].title, "Café");
    }
}
