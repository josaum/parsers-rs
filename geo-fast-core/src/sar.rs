//! SAR (Synthetic Aperture Radar) image parser.
//!
//! Reads ESA SAFE format (Sentinel-1) manifest and annotation XML,
//! and basic CEOS leader file format.
//!
//! Extracts: metadata, bounding box, polarization, GCP grid.

use std::borrow::Cow;

use crate::error::GeoError;
use crate::types::{BBox, Coordinate};
use crate::xml_text::TextRun;

/// Sets a reader up for the event loops of this module.
///
/// The reader does not trim text: a value arrives in pieces (see
/// [`TextRun`]), and trimming each piece would close up `a &amp; b` to `a&b`.
/// A `&` that starts no reference is reported as text, not as an error, so
/// one unescaped ampersand does not cost the whole document.
fn configure(reader: &mut quick_xml::Reader<&[u8]>) {
    let config = reader.config_mut();
    config.trim_text(false);
    config.allow_dangling_amp = true;
}

/// Metadata extracted from a SAR product.
#[derive(Debug, Clone)]
pub struct SarMetadata {
    /// Sensor name, e.g. "Sentinel-1A".
    pub sensor: String,
    /// ISO-8601 acquisition timestamp.
    pub acquisition_date: String,
    /// Polarization channels, e.g. `["VV", "VH"]`.
    pub polarization: Vec<String>,
    /// Geographic bounding box of the scene.
    pub bbox: BBox,
    /// Ground resolution in metres.
    pub resolution_m: f64,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Orbit direction: `"ascending"` or `"descending"`.
    pub orbit_direction: String,
    /// Mean incidence angle in degrees.
    pub incidence_angle_deg: f64,
    /// Product type code: `SLC`, `GRD`, or `OCN`.
    pub product_type: String,
}

/// A Ground Control Point mapping pixel (line, sample) to geographic position.
#[derive(Debug, Clone)]
pub struct GroundControlPoint {
    /// Geographic coordinate associated with this image-space sample.
    pub geo: Coordinate,
    /// Image line / row coordinate.
    pub line: f64,
    /// Image sample / column coordinate.
    pub sample: f64,
    /// Height above the ellipsoid in metres.
    pub height: f64,
}

/// The platform and the instrument a SAFE manifest names.
///
/// A manifest names the platform (`SENTINEL-1` and its number, `A`) and,
/// inside it, the instrument it carries (`Synthetic Aperture Radar`, with
/// the abbreviation `SAR`). [`SarMetadata::sensor`] is the platform name;
/// this keeps the instrument beside it instead of in its place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafePlatform {
    /// Platform family name followed by its number, such as `SENTINEL-1A`:
    /// the value of [`SarMetadata::sensor`].
    pub platform: String,
    /// Name of the instrument, such as `Synthetic Aperture Radar`, when the
    /// manifest names one.
    pub instrument: Option<String>,
    /// Abbreviation of the instrument's name, such as `SAR`, when the
    /// manifest gives one.
    pub instrument_abbreviation: Option<String>,
}

/// Parser for SAR metadata formats (SAFE manifest, CEOS leader, annotation GCPs).
pub struct SarParser;

impl SarParser {
    /// Parse SAR metadata from an ESA Sentinel-1 SAFE manifest.safe XML.
    ///
    /// The manifest contains `<metadataObject>` elements with platform info,
    /// acquisition dates, footprint coordinates, and product type.
    ///
    /// [`SarMetadata::sensor`] is the platform's family name followed by its
    /// number. The name of the instrument, which a manifest nests inside the
    /// platform, is not part of it: [`SarParser::parse_safe_platform`]
    /// returns that.
    ///
    /// # Errors
    ///
    /// Returns [`GeoError::Xml`] when the manifest is not well-formed and
    /// [`GeoError::InvalidFormat`] when it names no platform.
    pub fn parse_safe_manifest(xml_bytes: &[u8]) -> Result<SarMetadata, GeoError> {
        Self::parse_safe(xml_bytes).map(|(metadata, _)| metadata)
    }

    /// Reads the platform and instrument names from a SAFE manifest.
    ///
    /// # Errors
    ///
    /// As [`SarParser::parse_safe_manifest`].
    pub fn parse_safe_platform(xml_bytes: &[u8]) -> Result<SafePlatform, GeoError> {
        Self::parse_safe(xml_bytes).map(|(_, platform)| platform)
    }

    /// Reads a SAFE manifest once for both of its public readers.
    fn parse_safe(xml_bytes: &[u8]) -> Result<(SarMetadata, SafePlatform), GeoError> {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        // Bytes that are not UTF-8 read as U+FFFD; see `TextRun::new`.
        let xml = String::from_utf8_lossy(xml_bytes);
        let repaired = matches!(xml, Cow::Owned(_));
        let mut reader = Reader::from_reader(xml.as_bytes());
        configure(&mut reader);
        let mut buf = Vec::with_capacity(4096);
        let mut run = TextRun::new(repaired);

        let mut sensor = String::new();
        let mut acquisition_date = String::new();
        let mut polarization: Vec<String> = Vec::new();
        let mut orbit_direction = String::new();
        let mut product_type = String::new();
        let mut footprint_coords: Vec<Coordinate> = Vec::new();
        let mut resolution_m: f64 = 0.0;
        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut incidence_angle_deg: f64 = 0.0;

        // Track the current element path for context
        let mut current_tag = String::new();
        let mut in_platform = false;
        let mut in_instrument = false;
        let mut in_acquisition = false;
        let mut instrument: Option<String> = None;
        let mut instrument_abbreviation: Option<String> = None;

        loop {
            let event = reader.read_event_into(&mut buf).map_err(|e| {
                GeoError::Xml(format!(
                    "Error parsing SAFE manifest at position {}: {}",
                    reader.buffer_position(),
                    e
                ))
            })?;
            match &event {
                Event::Text(piece) => {
                    run.push_text(piece);
                    buf.clear();
                    continue;
                }
                Event::CData(section) => {
                    run.push_cdata(section);
                    buf.clear();
                    continue;
                }
                Event::GeneralRef(reference) => {
                    run.push_reference(reference);
                    buf.clear();
                    continue;
                }
                _ => {}
            }

            // Any other event ends the character data collected so far.
            let text = run.take().unwrap_or_default();
            let text = text.trim().to_string();
            if !text.is_empty() {
                match current_tag.as_str() {
                    // Platform / sensor
                    // The instrument is nested in the platform and has a
                    // `familyName` of its own, which is not the platform's.
                    "safe:familyName" | "familyName" if in_instrument => {
                        instrument = Some(text);
                    }
                    "safe:familyName" | "familyName" if in_platform => {
                        sensor = text;
                    }
                    "safe:number" | "number" if in_platform && !in_instrument => {
                        if !sensor.is_empty() {
                            sensor = format!("{}{}", sensor, text);
                        }
                    }
                    // Acquisition date
                    "safe:startTime" | "startTime" if in_acquisition => {
                        acquisition_date = text;
                    }
                    // Product type
                    "s1sarl1:productType" | "productType" => {
                        product_type = text;
                    }
                    // Polarization
                    "s1sarl1:transmitterReceiverPolarisation"
                    | "transmitterReceiverPolarisation" => {
                        if !polarization.contains(&text) {
                            polarization.push(text);
                        }
                    }
                    // Pass direction
                    "s1:pass" | "pass" => {
                        orbit_direction = text.to_lowercase();
                    }
                    // Resolution
                    "s1sarl1:resolution" | "resolution" => {
                        if let Ok(v) = text.parse::<f64>() {
                            resolution_m = v;
                        }
                    }
                    // Image dimensions
                    "s1sarl1:numberOfSamples" | "numberOfSamples" => {
                        if let Ok(v) = text.parse::<u32>() {
                            width = v;
                        }
                    }
                    "s1sarl1:numberOfLines" | "numberOfLines" => {
                        if let Ok(v) = text.parse::<u32>() {
                            height = v;
                        }
                    }
                    // Incidence angle
                    "s1sarl1:incidenceAngleMidSwath" | "incidenceAngleMidSwath" => {
                        if let Ok(v) = text.parse::<f64>() {
                            incidence_angle_deg = v;
                        }
                    }
                    // GML footprint coordinates (lat lon pairs)
                    "gml:coordinates" | "coordinates" => {
                        Self::parse_gml_coordinates(&text, &mut footprint_coords);
                    }
                    // posList variant
                    "gml:posList" | "posList" => {
                        Self::parse_gml_pos_list(&text, &mut footprint_coords);
                    }
                    _ => {}
                }
            }

            match &event {
                Event::Start(e) => {
                    let tag_name = e.name().as_ref().to_string();
                    match tag_name.as_str() {
                        "safe:platform" | "platform" => in_platform = true,
                        "safe:instrument" | "instrument" if in_platform => in_instrument = true,
                        "safe:familyName" | "familyName" if in_instrument => {
                            let abbreviation = e
                                .attributes()
                                .with_checks(false)
                                .flatten()
                                .find(|attr| attr.key.local_name().as_ref() == "abbreviation")
                                .and_then(|attr| {
                                    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                        .ok()
                                })
                                // A value that was not UTF-8 is unreadable.
                                .filter(|value| {
                                    !(repaired && value.contains(char::REPLACEMENT_CHARACTER))
                                })
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            if abbreviation.is_some() {
                                instrument_abbreviation = abbreviation;
                            }
                        }
                        "safe:acquisitionPeriod" | "acquisitionPeriod" => in_acquisition = true,
                        _ => {}
                    }
                    current_tag = tag_name;
                }
                Event::End(e) => {
                    let tag_name = e.name().as_ref().to_string();
                    match tag_name.as_str() {
                        "safe:platform" | "platform" => {
                            in_platform = false;
                            in_instrument = false;
                        }
                        "safe:instrument" | "instrument" => in_instrument = false,
                        "safe:acquisitionPeriod" | "acquisitionPeriod" => in_acquisition = false,
                        _ => {}
                    }
                    current_tag.clear();
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        if sensor.is_empty() {
            return Err(GeoError::InvalidFormat(
                "No platform/sensor found in SAFE manifest".into(),
            ));
        }

        let bbox = if footprint_coords.is_empty() {
            BBox::new(0.0, 0.0, 0.0, 0.0)
        } else {
            let min_lon = footprint_coords
                .iter()
                .map(|p| p.lon)
                .fold(f64::INFINITY, f64::min);
            let max_lon = footprint_coords
                .iter()
                .map(|p| p.lon)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_lat = footprint_coords
                .iter()
                .map(|p| p.lat)
                .fold(f64::INFINITY, f64::min);
            let max_lat = footprint_coords
                .iter()
                .map(|p| p.lat)
                .fold(f64::NEG_INFINITY, f64::max);
            BBox::new(min_lon, min_lat, max_lon, max_lat)
        };

        let platform = SafePlatform {
            platform: sensor.clone(),
            instrument,
            instrument_abbreviation,
        };
        let metadata = SarMetadata {
            sensor,
            acquisition_date,
            polarization,
            bbox,
            resolution_m,
            width,
            height,
            orbit_direction,
            incidence_angle_deg,
            product_type,
        };
        Ok((metadata, platform))
    }

    /// Parse SAR metadata from a CEOS leader file.
    ///
    /// CEOS (Committee on Earth Observation Satellites) format stores metadata
    /// as fixed-width ASCII records.  The leader file has record type 10
    /// (File Descriptor) followed by record type 20 (Scene Header).
    pub fn parse_ceos_leader(data: &[u8]) -> Result<SarMetadata, GeoError> {
        // CEOS leader files are structured as fixed-length records.
        // Record header: 12 bytes (sequence, type-code sub1-3, length)
        // Scene header record starts after the file descriptor record.
        if data.len() < 720 {
            return Err(GeoError::InvalidFormat(
                "CEOS leader file too short (need at least 720 bytes)".into(),
            ));
        }

        // File Descriptor Record length is in bytes 8-11 (big-endian u32)
        let fdr_len = Self::read_be_u32(data, 8) as usize;
        if fdr_len == 0 || fdr_len >= data.len() {
            return Err(GeoError::InvalidFormat(
                "Invalid CEOS file descriptor record length".into(),
            ));
        }

        let scene = &data[fdr_len..];
        if scene.len() < 720 {
            return Err(GeoError::InvalidFormat(
                "CEOS scene header record too short".into(),
            ));
        }

        // Extract ASCII fields from the scene header record.
        // Offsets are 0-based within the scene record.
        let sensor = Self::ceos_ascii(scene, 20, 16);
        let acquisition_date = Self::ceos_ascii(scene, 68, 32);
        let product_type = Self::ceos_ascii(scene, 228, 16);
        let orbit_direction = Self::ceos_ascii(scene, 100, 16).to_lowercase();

        let width = Self::ceos_int(scene, 248, 8) as u32;
        let height = Self::ceos_int(scene, 256, 8) as u32;
        let resolution_m = Self::ceos_float(scene, 264, 16);
        let incidence_angle_deg = Self::ceos_float(scene, 484, 8);

        // Corner coordinates: 4 lat/lon pairs starting at offset 116
        // Each is 16 chars (lat then lon), 8 chars each
        let mut corners: Vec<Coordinate> = Vec::new();
        for i in 0..4 {
            let base = 116 + i * 16;
            let lat = Self::ceos_float(scene, base, 8);
            let lon = Self::ceos_float(scene, base + 8, 8);
            if lat != 0.0 || lon != 0.0 {
                corners.push(Coordinate::new(lon, lat));
            }
        }

        let bbox = if corners.is_empty() {
            BBox::new(0.0, 0.0, 0.0, 0.0)
        } else {
            let min_lon = corners.iter().map(|p| p.lon).fold(f64::INFINITY, f64::min);
            let max_lon = corners
                .iter()
                .map(|p| p.lon)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_lat = corners.iter().map(|p| p.lat).fold(f64::INFINITY, f64::min);
            let max_lat = corners
                .iter()
                .map(|p| p.lat)
                .fold(f64::NEG_INFINITY, f64::max);
            BBox::new(min_lon, min_lat, max_lon, max_lat)
        };

        // Polarization is typically embedded in the data set summary
        // For basic CEOS we report it as unknown
        let polarization = Vec::new();

        Ok(SarMetadata {
            sensor,
            acquisition_date,
            polarization,
            bbox,
            resolution_m,
            width,
            height,
            orbit_direction,
            incidence_angle_deg,
            product_type,
        })
    }

    /// Extract Ground Control Points from a Sentinel-1 annotation XML.
    ///
    /// Annotation files contain `<geolocationGridPoint>` elements with
    /// `<line>`, `<pixel>`, `<latitude>`, `<longitude>`, `<height>`.
    pub fn extract_gcps(annotation_xml: &[u8]) -> Result<Vec<GroundControlPoint>, GeoError> {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        // Bytes that are not UTF-8 read as U+FFFD; see `TextRun::new`.
        let xml = String::from_utf8_lossy(annotation_xml);
        let mut reader = Reader::from_reader(xml.as_bytes());
        configure(&mut reader);
        let mut buf = Vec::with_capacity(2048);
        let mut run = TextRun::new(matches!(xml, Cow::Owned(_)));

        let mut gcps: Vec<GroundControlPoint> = Vec::new();

        let mut in_gcp = false;
        let mut current_tag = String::new();
        let mut gcp_line: f64 = 0.0;
        let mut gcp_sample: f64 = 0.0;
        let mut gcp_lat: f64 = 0.0;
        let mut gcp_lon: f64 = 0.0;
        let mut gcp_height: f64 = 0.0;

        loop {
            let event = reader.read_event_into(&mut buf).map_err(|e| {
                GeoError::Xml(format!(
                    "Error parsing annotation XML at position {}: {}",
                    reader.buffer_position(),
                    e
                ))
            })?;
            match &event {
                Event::Text(piece) => {
                    run.push_text(piece);
                    buf.clear();
                    continue;
                }
                Event::CData(section) => {
                    run.push_cdata(section);
                    buf.clear();
                    continue;
                }
                Event::GeneralRef(reference) => {
                    run.push_reference(reference);
                    buf.clear();
                    continue;
                }
                _ => {}
            }

            // Any other event ends the character data collected so far.
            if let Some(text) = run.take().filter(|_| in_gcp) {
                let text = text.trim();
                match current_tag.as_str() {
                    "line" => gcp_line = text.parse().unwrap_or(0.0),
                    "pixel" => gcp_sample = text.parse().unwrap_or(0.0),
                    "latitude" => gcp_lat = text.parse().unwrap_or(0.0),
                    "longitude" => gcp_lon = text.parse().unwrap_or(0.0),
                    "height" => gcp_height = text.parse().unwrap_or(0.0),
                    _ => {}
                }
            }

            match &event {
                Event::Start(e) => {
                    let tag = e.name().as_ref().to_string();
                    if tag == "geolocationGridPoint" {
                        in_gcp = true;
                        gcp_line = 0.0;
                        gcp_sample = 0.0;
                        gcp_lat = 0.0;
                        gcp_lon = 0.0;
                        gcp_height = 0.0;
                    }
                    current_tag = tag;
                }
                Event::End(e) => {
                    let tag = e.name().as_ref().to_string();
                    if tag == "geolocationGridPoint" && in_gcp {
                        gcps.push(GroundControlPoint {
                            geo: Coordinate::new(gcp_lon, gcp_lat),
                            line: gcp_line,
                            sample: gcp_sample,
                            height: gcp_height,
                        });
                        in_gcp = false;
                    }
                    current_tag.clear();
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(gcps)
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Parse GML `coordinates` element: "lat,lon lat,lon ..." (space-separated)
    fn parse_gml_coordinates(text: &str, out: &mut Vec<Coordinate>) {
        for pair in text.split_whitespace() {
            let parts: Vec<&str> = pair.split(',').collect();
            if parts.len() >= 2 {
                if let (Ok(lat), Ok(lon)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    out.push(Coordinate::new(lon, lat));
                }
            }
        }
    }

    /// Parse GML `posList`: "lat lon lat lon ..." (flat list, pairs)
    fn parse_gml_pos_list(text: &str, out: &mut Vec<Coordinate>) {
        let nums: Vec<f64> = text
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        for chunk in nums.chunks(2) {
            if chunk.len() == 2 {
                out.push(Coordinate::new(chunk[1], chunk[0])); // lat, lon order
            }
        }
    }

    /// Read a big-endian u32 from data at offset.
    fn read_be_u32(data: &[u8], offset: usize) -> u32 {
        if offset + 4 > data.len() {
            return 0;
        }
        u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    /// Read a trimmed ASCII string from CEOS fixed-width field.
    fn ceos_ascii(data: &[u8], offset: usize, len: usize) -> String {
        if offset + len > data.len() {
            return String::new();
        }
        String::from_utf8_lossy(&data[offset..offset + len])
            .trim()
            .to_string()
    }

    /// Read a CEOS integer field (ASCII decimal, fixed width).
    fn ceos_int(data: &[u8], offset: usize, len: usize) -> i64 {
        Self::ceos_ascii(data, offset, len)
            .parse::<i64>()
            .unwrap_or(0)
    }

    /// Read a CEOS floating-point field (ASCII, fixed width).
    fn ceos_float(data: &[u8], offset: usize, len: usize) -> f64 {
        Self::ceos_ascii(data, offset, len)
            .parse::<f64>()
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_safe_manifest_minimal() {
        let xml = br#"<?xml version="1.0"?>
<xfdu:XFDU xmlns:xfdu="urn:ccsds:schema:xfdu:1"
            xmlns:safe="http://www.esa.int/safe/sentinel-1.0"
            xmlns:s1sarl1="http://www.esa.int/safe/sentinel-1.0/sentinel-1/sar/level-1"
            xmlns:s1="http://www.esa.int/safe/sentinel-1.0/sentinel-1"
            xmlns:gml="http://www.opengis.net/gml">
  <metadataSection>
    <metadataObject>
      <metadataWrap>
        <xmlData>
          <safe:platform>
            <safe:familyName>SENTINEL-1</safe:familyName>
            <safe:number>A</safe:number>
          </safe:platform>
        </xmlData>
      </metadataWrap>
    </metadataObject>
    <metadataObject>
      <metadataWrap>
        <xmlData>
          <safe:acquisitionPeriod>
            <safe:startTime>2024-01-15T10:30:00.000Z</safe:startTime>
          </safe:acquisitionPeriod>
        </xmlData>
      </metadataWrap>
    </metadataObject>
    <metadataObject>
      <metadataWrap>
        <xmlData>
          <s1sarl1:productType>GRD</s1sarl1:productType>
          <s1sarl1:transmitterReceiverPolarisation>VV</s1sarl1:transmitterReceiverPolarisation>
          <s1sarl1:transmitterReceiverPolarisation>VH</s1sarl1:transmitterReceiverPolarisation>
          <s1:pass>ASCENDING</s1:pass>
          <gml:coordinates>-10.0,-60.0 -10.0,-58.0 -12.0,-58.0 -12.0,-60.0</gml:coordinates>
        </xmlData>
      </metadataWrap>
    </metadataObject>
  </metadataSection>
</xfdu:XFDU>"#;

        let meta = SarParser::parse_safe_manifest(xml).unwrap();
        assert_eq!(meta.sensor, "SENTINEL-1A");
        assert_eq!(meta.acquisition_date, "2024-01-15T10:30:00.000Z");
        assert_eq!(meta.polarization, vec!["VV", "VH"]);
        assert_eq!(meta.product_type, "GRD");
        assert_eq!(meta.orbit_direction, "ascending");
        assert!(meta.bbox.min_lon < meta.bbox.max_lon);
    }

    #[test]
    fn test_extract_gcps() {
        let xml = br#"<?xml version="1.0"?>
<product>
  <geolocationGrid>
    <geolocationGridPointList>
      <geolocationGridPoint>
        <line>0</line>
        <pixel>0</pixel>
        <latitude>-10.5</latitude>
        <longitude>-59.2</longitude>
        <height>150.0</height>
      </geolocationGridPoint>
      <geolocationGridPoint>
        <line>100</line>
        <pixel>200</pixel>
        <latitude>-11.0</latitude>
        <longitude>-58.8</longitude>
        <height>120.0</height>
      </geolocationGridPoint>
    </geolocationGridPointList>
  </geolocationGrid>
</product>"#;

        let gcps = SarParser::extract_gcps(xml).unwrap();
        assert_eq!(gcps.len(), 2);
        assert!((gcps[0].geo.lat - (-10.5)).abs() < 1e-6);
        assert!((gcps[1].sample - 200.0).abs() < 1e-6);
    }

    /// A Sentinel-1 manifest names the instrument inside the platform, in a
    /// `familyName` of its own, which replaced the platform's name as the
    /// sensor: "Synthetic Aperture Radar" for "SENTINEL-1A".
    const NESTED_INSTRUMENT: &[u8] = br#"<xfdu:XFDU><safe:platform>
        <safe:nssdcIdentifier>2014-016A</safe:nssdcIdentifier>
        <safe:familyName>SENTINEL-1</safe:familyName>
        <safe:number>A</safe:number>
        <safe:instrument>
          <safe:familyName abbreviation="SAR">Synthetic Aperture Radar</safe:familyName>
          <safe:number>9</safe:number>
        </safe:instrument>
      </safe:platform></xfdu:XFDU>"#;

    #[test]
    fn instrument_name_inside_the_platform_does_not_replace_the_platform_name() {
        let metadata = SarParser::parse_safe_manifest(NESTED_INSTRUMENT).expect("manifest");
        assert_eq!(metadata.sensor, "SENTINEL-1A");

        let platform = SarParser::parse_safe_platform(NESTED_INSTRUMENT).expect("platform");
        assert_eq!(
            platform,
            SafePlatform {
                platform: "SENTINEL-1A".into(),
                instrument: Some("Synthetic Aperture Radar".into()),
                instrument_abbreviation: Some("SAR".into()),
            }
        );
        // A manifest that names no instrument has none.
        let bare = SarParser::parse_safe_platform(&manifest("ERS", "")).expect("platform");
        assert_eq!(bare.platform, "ERS");
        assert_eq!(
            (bare.instrument, bare.instrument_abbreviation),
            (None, None)
        );
    }

    /// A SAFE manifest whose platform name is `family_name`, with `extra`
    /// elements after the platform.
    fn manifest(family_name: &str, extra: &str) -> Vec<u8> {
        format!(
            "<x><safe:platform><safe:familyName>{family_name}</safe:familyName>\
             </safe:platform>{extra}</x>"
        )
        .into_bytes()
    }

    fn sensor(family_name: &str) -> Result<String, String> {
        SarParser::parse_safe_manifest(&manifest(family_name, ""))
            .map(|metadata| metadata.sensor)
            .map_err(|error| error.to_string())
    }

    /// The values quick-xml 0.36 produced, when a reference was part of the
    /// text event around it. From 0.41 a reference is an event of its own and
    /// a value arrives in pieces; assigning per piece would keep ` B` of
    /// `A &amp; B` and `.5` of `1&#48;.5`.
    #[test]
    fn references_in_a_platform_name_are_resolved_in_place() {
        for (written, expected) in [
            ("A &amp; B", "A & B"),
            ("Caf&#233;", "Café"),
            ("Caf&#xE9;", "Café"),
            ("&amp;", "&"),
            ("a&lt;b", "a<b"),
            ("  padded &amp;  ", "padded &"),
            ("&#32;x&#32;", "x"),
            ("1&#48;.5", "10.5"),
        ] {
            assert_eq!(sensor(written).as_deref(), Ok(expected), "{written}");
        }
    }

    /// A CDATA section used to be skipped: a platform named only in one was
    /// "no platform", and `pre<![CDATA[mid]]>post` was read as `post`. It is
    /// text like the rest of the value, taken as written. Character data on
    /// either side of a comment is still one stretch at a time, the last one
    /// winning.
    #[test]
    fn cdata_in_a_platform_name_is_read_as_text() {
        assert_eq!(sensor("<![CDATA[X &amp; Y]]>").as_deref(), Ok("X &amp; Y"));
        assert_eq!(sensor("<![CDATA[A < B]]>").as_deref(), Ok("A < B"));
        assert_eq!(
            sensor("pre<![CDATA[mid]]>post").as_deref(),
            Ok("premidpost")
        );
        assert_eq!(sensor(" a <![CDATA[ & ]]> b ").as_deref(), Ok("a  &  b"));
        assert!(sensor("<![CDATA[]]>").is_err());
        assert_eq!(sensor("a<!-- c -->b").as_deref(), Ok("b"));
        assert!(sensor("   ").is_err());
    }

    #[test]
    fn numeric_field_written_with_a_reference_is_read_whole() {
        let parsed = SarParser::parse_safe_manifest(&manifest(
            "S1",
            "<resolution>1&#48;.5</resolution>\
             <numberOfSamples> 2&#53;0 </numberOfSamples>\
             <numberOfLines>&#49;&#48;</numberOfLines>\
             <incidenceAngleMidSwath>3&#57;&#46;25</incidenceAngleMidSwath>\
             <pass>ASC&#69;NDING</pass>\
             <coordinates>-1&#48;.0,-60.0 -12.0,-58.0</coordinates>",
        ))
        .expect("manifest");
        assert_eq!(parsed.resolution_m, 10.5);
        assert_eq!(parsed.width, 250);
        assert_eq!(parsed.height, 10);
        assert_eq!(parsed.incidence_angle_deg, 39.25);
        assert_eq!(parsed.orbit_direction, "ascending");
        assert_eq!(parsed.bbox, BBox::new(-60.0, -12.0, -58.0, -10.0));
        // A numeric field in a CDATA section is read too.
        let parsed = SarParser::parse_safe_manifest(&manifest(
            "S1",
            "<resolution><![CDATA[10.5]]></resolution>",
        ))
        .expect("manifest");
        assert_eq!(parsed.resolution_m, 10.5);
    }

    #[test]
    fn ground_control_point_written_with_references_is_read_whole() {
        let annotation = b"<p><geolocationGridPoint>\
            <line>1&#48;0</line><pixel> 2&#48;0 </pixel>\
            <latitude>-1&#48;.5</latitude><longitude>&#45;59&#46;25</longitude>\
            <height><![CDATA[150.0]]></height>\
            </geolocationGridPoint></p>";
        let points = SarParser::extract_gcps(annotation).expect("annotation");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].line, 100.0);
        assert_eq!(points[0].sample, 200.0);
        assert_eq!(points[0].geo.lat, -10.5);
        assert_eq!(points[0].geo.lon, -59.25);
        assert_eq!(points[0].height, 150.0);
    }

    /// quick-xml 0.36 failed to unescape such a value and the reader then
    /// dropped it whole: a platform named `A &unknown; B` was "no platform".
    #[test]
    fn reference_that_cannot_be_resolved_is_kept_as_written() {
        for written in ["A &unknown; B", "A & B", "A &#0; B", "AT&T"] {
            assert_eq!(sensor(written).as_deref(), Ok(written), "{written}");
        }
    }
}
