//! Documents that are not UTF-8 read as they did before quick-xml 0.42.
//!
//! quick-xml 0.42 refuses a whole document at its first byte that is not
//! UTF-8. The SAR readers read such a byte as U+FFFD instead, and keep
//! treating a value holding one as unreadable, as they did when the reader
//! handed them raw bytes. The expected values below were taken from the
//! readers on quick-xml 0.41 and must not change.

use geo_fast_core::sar::SarParser;

/// A SAFE manifest in ISO-8859-1 with Latin-1 bytes in an element's text and
/// in an attribute value.
const LATIN1_MANIFEST: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>
<xfdu:XFDU xmlns:xfdu=\"urn:ccsds:schema:xfdu:1\" xmlns:safe=\"http://www.esa.int/safe/sentinel-1.0\">
  <safe:platform>
    <safe:familyName>SENTINEL-1</safe:familyName>
    <safe:number>A</safe:number>
    <safe:instrument>
      <safe:familyName abbreviation=\"SAR\xe9\">Synthetic Aperture Radar \xe9</safe:familyName>
    </safe:instrument>
  </safe:platform>
  <safe:acquisitionPeriod><safe:startTime>2024-01-02T03:04:05</safe:startTime></safe:acquisitionPeriod>
  <s1sarl1:productType xmlns:s1sarl1=\"x\">GR\xe9D</s1sarl1:productType>
  <s1:pass xmlns:s1=\"y\">ASC\xc9NDING</s1:pass>
</xfdu:XFDU>
";

/// An annotation in ISO-8859-1 with a Latin-1 byte in one value.
const LATIN1_ANNOTATION: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>
<product><geolocationGrid><geolocationGridPointList>
  <geolocationGridPoint><line>1</line><pixel>2</pixel><latitude>3\xb0</latitude><longitude>4</longitude><height>5</height></geolocationGridPoint>
</geolocationGridPointList></geolocationGrid></product>
";

#[test]
fn latin_1_manifest_reads_as_before() {
    let metadata = format!("{:?}", SarParser::parse_safe_manifest(LATIN1_MANIFEST));
    let platform = format!("{:?}", SarParser::parse_safe_platform(LATIN1_MANIFEST));
    assert_eq!(format!("{metadata}\n{platform}"), EXPECTED_MANIFEST);
}

#[test]
fn latin_1_annotation_reads_as_before() {
    let gcps = format!("{:?}", SarParser::extract_gcps(LATIN1_ANNOTATION));
    assert_eq!(gcps, EXPECTED_ANNOTATION);
}

/// A capabilities document that was served as ISO-8859-1 reaches the parser
/// already decoded, its declaration still naming ISO-8859-1.
#[cfg(feature = "http")]
#[test]
fn capabilities_declared_latin_1_read_as_before() {
    let xml = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n\
        <WMS_Capabilities><Capability><Layer><Name>caf\u{e9}</Name>\
        <Title>Caf\u{e9} \u{e0} la carte</Title><CRS>EPSG:4326</CRS></Layer>\
        </Capability></WMS_Capabilities>";
    let layers = format!("{:?}", geo_fast_core::wms::parse_capabilities_xml(xml));
    assert_eq!(layers, EXPECTED_CAPABILITIES);
}

const EXPECTED_MANIFEST: &str = "Ok(SarMetadata { sensor: \"SENTINEL-1A\", acquisition_date: \"2024-01-02T03:04:05\", polarization: [], bbox: BBox { min_lon: 0.0, max_lon: 0.0, min_lat: 0.0, max_lat: 0.0 }, resolution_m: 0.0, width: 0, height: 0, orbit_direction: \"\", incidence_angle_deg: 0.0, product_type: \"\" })\nOk(SafePlatform { platform: \"SENTINEL-1A\", instrument: None, instrument_abbreviation: None })";
const EXPECTED_ANNOTATION: &str = "Ok([GroundControlPoint { geo: Coordinate { lon: 4.0, lat: 0.0 }, line: 1.0, sample: 2.0, height: 5.0 }])";
#[cfg(feature = "http")]
const EXPECTED_CAPABILITIES: &str = "Ok([WmsLayer { name: \"café\", title: \"Café à la carte\", crs_list: [\"EPSG:4326\"], bbox: None }])";

/// A U+FFFD a manifest really holds is read as text even when a stray byte
/// sits elsewhere in the document; a stray byte in the value itself still
/// makes it unreadable.
#[test]
fn replacement_character_beside_a_stray_byte_is_kept() {
    let manifest = "<xfdu:XFDU xmlns:xfdu=\"urn:ccsds:schema:xfdu:1\" xmlns:safe=\"http://www.esa.int/safe/sentinel-1.0\">\
        <!-- ~ -->\
        <safe:platform><safe:familyName>SENTINEL-1</safe:familyName><safe:number>A</safe:number>\
        <safe:instrument><safe:familyName abbreviation=\"S\u{FFFD}R\">Radar \u{FFFD}</safe:familyName></safe:instrument>\
        </safe:platform>\
        <s1sarl1:productType xmlns:s1sarl1=\"x\">GR\u{FFFD}D</s1sarl1:productType>\
        <s1:pass xmlns:s1=\"y\">ASC~NDING</s1:pass>\
        </xfdu:XFDU>";
    let bytes: Vec<u8> = manifest
        .bytes()
        .map(|b| if b == b'~' { 0xFF } else { b })
        .collect();
    let metadata = format!("{:?}", SarParser::parse_safe_manifest(&bytes));
    let platform = format!("{:?}", SarParser::parse_safe_platform(&bytes));
    let output = format!("{metadata}\n{platform}");
    assert_eq!(output, EXPECTED_REPLACEMENT_KEPT);
}

const EXPECTED_REPLACEMENT_KEPT: &str = "Ok(SarMetadata { sensor: \"SENTINEL-1A\", acquisition_date: \"\", polarization: [], bbox: BBox { min_lon: 0.0, max_lon: 0.0, min_lat: 0.0, max_lat: 0.0 }, resolution_m: 0.0, width: 0, height: 0, orbit_direction: \"\", incidence_angle_deg: 0.0, product_type: \"GR\u{FFFD}D\" })\nOk(SafePlatform { platform: \"SENTINEL-1A\", instrument: Some(\"Radar \u{FFFD}\"), instrument_abbreviation: Some(\"S\u{FFFD}R\") })";
