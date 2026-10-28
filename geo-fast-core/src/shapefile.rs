//! ESRI Shapefile parser (.shp + .dbf companion).
//!
//! Reference: ESRI Shapefile Technical Description (July 1998).
//!
//! Supported shape types (XY geometry is always returned; Z/M are optional
//! measure arrays attached via feature properties when present):
//!   0  Null Shape
//!   1  Point / 11 PointZ / 21 PointM
//!   3  PolyLine / 13 PolyLineZ / 23 PolyLineM
//!   5  Polygon / 15 PolygonZ / 25 PolygonM
//!   8  MultiPoint / 18 MultiPointZ / 28 MultiPointM
//!
//! Z values are stored on the feature as `properties["_z"]` (scalar or array).
//! M values are stored as `properties["_m"]`. Core [`Geometry`] remains 2-D
//! (lon/lat) so WKT / Milvus interchange stays XY-only.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use serde_json::{json, Map as JsonMap, Value};

use crate::error::GeoError;
use crate::types::{BBox, Coordinate, GeoFeature, Geometry};

// ── Shape type codes ─────────────────────────────────────────────────────────
const SHP_NULL: i32 = 0;
const SHP_POINT: i32 = 1;
const SHP_POLYLINE: i32 = 3;
const SHP_POLYGON: i32 = 5;
const SHP_MULTIPOINT: i32 = 8;
const SHP_POINT_Z: i32 = 11;
const SHP_POLYLINE_Z: i32 = 13;
const SHP_POLYGON_Z: i32 = 15;
const SHP_MULTIPOINT_Z: i32 = 18;
const SHP_POINT_M: i32 = 21;
const SHP_POLYLINE_M: i32 = 23;
const SHP_POLYGON_M: i32 = 25;
const SHP_MULTIPOINT_M: i32 = 28;

// ── dBASE field types ────────────────────────────────────────────────────────
const DBF_HEADER_SIZE: usize = 32;
const DBF_FIELD_DESCRIPTOR_SIZE: usize = 32;
const DBF_TERMINATOR: u8 = 0x0D;

/// Field definition from the .dbf attribute table.
#[derive(Debug, Clone)]
pub struct FieldDef {
    /// Field name (≤10 chars in dBASE III).
    pub name: String,
    /// dBASE type character (`C`, `N`, `F`, `L`, `D`, …).
    pub field_type: char,
    /// Field width in bytes.
    pub length: u8,
    /// Decimal places for numeric fields.
    pub decimal_count: u8,
}

/// Complete shapefile dataset.
#[derive(Debug)]
pub struct ShapefileData {
    /// File-level bounding box from the .shp header.
    pub bbox: BBox,
    /// Parsed features (geometry + attributes).
    pub features: Vec<GeoFeature>,
    /// Attribute schema from the companion .dbf (may be empty).
    pub schema: Vec<FieldDef>,
}

/// Optional Z/M arrays read from a Z or M shape record.
#[derive(Debug, Default)]
struct MeasureArrays {
    z: Option<Vec<f64>>,
    m: Option<Vec<f64>>,
}

type ShpRecord = (i32, Geometry, MeasureArrays);

/// Parse a shapefile from disk.
///
/// `shp_path` must end in `.shp`; the matching `.dbf` is inferred automatically.
/// If the `.dbf` does not exist, features will have empty `properties` (aside
/// from optional `_z` / `_m` from the shape record).
pub fn parse_shapefile<P: AsRef<Path>>(shp_path: P) -> Result<ShapefileData, GeoError> {
    let shp_path = shp_path.as_ref();
    let shp_bytes = std::fs::read(shp_path).map_err(GeoError::Io)?;

    let dbf_path = shp_path.with_extension("dbf");
    let dbf_bytes = if dbf_path.exists() {
        Some(std::fs::read(&dbf_path).map_err(GeoError::Io)?)
    } else {
        None
    };

    parse_shapefile_bytes(&shp_bytes, dbf_bytes.as_deref())
}

/// Parse from raw byte slices (useful for in-memory / zip extraction).
pub fn parse_shapefile_bytes(shp: &[u8], dbf: Option<&[u8]>) -> Result<ShapefileData, GeoError> {
    let (bbox, raw_records) = parse_shp(shp)?;
    let (schema, attribute_rows) = if let Some(dbf_bytes) = dbf {
        parse_dbf(dbf_bytes)?
    } else {
        (Vec::new(), Vec::new())
    };

    let features: Vec<GeoFeature> = raw_records
        .into_iter()
        .enumerate()
        .map(|(i, (record_num, geom, measures))| {
            let mut map = JsonMap::new();
            if let Some(row) = attribute_rows.get(i) {
                for (field, val) in schema.iter().zip(row.iter()) {
                    map.insert(field.name.clone(), Value::String(val.clone()));
                }
            }
            attach_measures(&mut map, &measures);
            GeoFeature::new(Some(record_num.to_string()), geom, Value::Object(map))
        })
        .collect();

    Ok(ShapefileData {
        bbox,
        features,
        schema,
    })
}

fn attach_measures(map: &mut JsonMap<String, Value>, measures: &MeasureArrays) {
    if let Some(ref z) = measures.z {
        if z.len() == 1 {
            map.insert("_z".into(), json!(z[0]));
        } else if !z.is_empty() {
            map.insert("_z".into(), json!(z));
        }
    }
    if let Some(ref m) = measures.m {
        if m.len() == 1 {
            map.insert("_m".into(), json!(m[0]));
        } else if !m.is_empty() {
            map.insert("_m".into(), json!(m));
        }
    }
}

// ── .shp parser ──────────────────────────────────────────────────────────────

fn parse_shp(data: &[u8]) -> Result<(BBox, Vec<ShpRecord>), GeoError> {
    if data.len() < 100 {
        return Err(GeoError::InvalidFormat("SHP file too short".into()));
    }
    let mut cur = Cursor::new(data);

    // File header (100 bytes)
    let file_code = cur.read_i32::<BigEndian>().map_err(io_err)?;
    if file_code != 9994 {
        return Err(GeoError::InvalidFormat(format!(
            "Invalid SHP file code: {file_code}"
        )));
    }
    cur.seek(SeekFrom::Current(20)).map_err(io_err)?; // skip unused fields
    let _file_length = cur.read_i32::<BigEndian>().map_err(io_err)?; // in 16-bit words
    let _version = cur.read_i32::<LittleEndian>().map_err(io_err)?;
    let _shape_type = cur.read_i32::<LittleEndian>().map_err(io_err)?;

    let min_x = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let min_y = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let max_x = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let max_y = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    // Skip Z and M ranges (32 bytes)
    cur.seek(SeekFrom::Current(32)).map_err(io_err)?;

    let bbox = BBox::new(min_x, min_y, max_x, max_y);

    let mut records = Vec::new();
    while (cur.position() as usize) < data.len() {
        // Record header
        let record_num = match cur.read_i32::<BigEndian>() {
            Ok(n) => n,
            Err(_) => break,
        };
        let content_len_words = cur.read_i32::<BigEndian>().map_err(io_err)?; // 16-bit words
        let content_start = cur.position() as usize;
        let content_end = content_start + (content_len_words as usize) * 2;

        let shape_type = cur.read_i32::<LittleEndian>().map_err(io_err)?;
        match shape_type {
            SHP_NULL => {
                // Nothing else in a null record.
            }
            SHP_POINT | SHP_POINT_Z | SHP_POINT_M => {
                let (geom, measures) = read_point(&mut cur, shape_type, content_end)?;
                records.push((record_num, geom, measures));
            }
            SHP_MULTIPOINT | SHP_MULTIPOINT_Z | SHP_MULTIPOINT_M => {
                let (geom, measures) = read_multipoint(&mut cur, shape_type, content_end)?;
                records.push((record_num, geom, measures));
            }
            SHP_POLYLINE | SHP_POLYLINE_Z | SHP_POLYLINE_M | SHP_POLYGON | SHP_POLYGON_Z
            | SHP_POLYGON_M => {
                let (geom, measures) = read_poly(&mut cur, shape_type, content_end)?;
                records.push((record_num, geom, measures));
            }
            other => {
                return Err(GeoError::UnsupportedFormat(format!(
                    "Unsupported shape type: {other}"
                )));
            }
        }
        // Advance to end of record (guards against optional-M undershoot / overshoot).
        if (cur.position() as usize) != content_end && content_end <= data.len() {
            cur.seek(SeekFrom::Start(content_end as u64))
                .map_err(io_err)?;
        }
    }
    Ok((bbox, records))
}

fn read_point(
    cur: &mut Cursor<&[u8]>,
    shape_type: i32,
    content_end: usize,
) -> Result<(Geometry, MeasureArrays), GeoError> {
    let lon = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let lat = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let mut measures = MeasureArrays::default();
    match shape_type {
        SHP_POINT_Z => {
            let z = cur.read_f64::<LittleEndian>().map_err(io_err)?;
            measures.z = Some(vec![z]);
            if bytes_remaining(cur, content_end) >= 8 {
                let m = cur.read_f64::<LittleEndian>().map_err(io_err)?;
                if m.is_finite() {
                    measures.m = Some(vec![m]);
                }
            }
        }
        SHP_POINT_M if bytes_remaining(cur, content_end) >= 8 => {
            let m = cur.read_f64::<LittleEndian>().map_err(io_err)?;
            if m.is_finite() {
                measures.m = Some(vec![m]);
            }
        }
        _ => {}
    }
    Ok((Geometry::Point(Coordinate::new(lon, lat)), measures))
}

fn read_multipoint(
    cur: &mut Cursor<&[u8]>,
    shape_type: i32,
    content_end: usize,
) -> Result<(Geometry, MeasureArrays), GeoError> {
    // Bounding box (skip)
    cur.seek(SeekFrom::Current(32)).map_err(io_err)?;
    let num_points = cur.read_i32::<LittleEndian>().map_err(io_err)? as usize;
    let mut pts = Vec::with_capacity(num_points);
    for _ in 0..num_points {
        let lon = cur.read_f64::<LittleEndian>().map_err(io_err)?;
        let lat = cur.read_f64::<LittleEndian>().map_err(io_err)?;
        pts.push(Coordinate::new(lon, lat));
    }
    let mut measures = MeasureArrays::default();
    match shape_type {
        SHP_MULTIPOINT_Z => {
            measures.z = Some(read_measure_array(cur, num_points)?);
            measures.m = read_optional_measure_array(cur, num_points, content_end)?;
        }
        SHP_MULTIPOINT_M => {
            measures.m = Some(read_measure_array(cur, num_points)?);
        }
        _ => {}
    }
    Ok((Geometry::MultiPoint(pts), measures))
}

fn read_poly(
    cur: &mut Cursor<&[u8]>,
    shape_type: i32,
    content_end: usize,
) -> Result<(Geometry, MeasureArrays), GeoError> {
    // Bounding box (skip)
    cur.seek(SeekFrom::Current(32)).map_err(io_err)?;
    let num_parts = cur.read_i32::<LittleEndian>().map_err(io_err)? as usize;
    let num_points = cur.read_i32::<LittleEndian>().map_err(io_err)? as usize;

    let mut part_starts = Vec::with_capacity(num_parts);
    for _ in 0..num_parts {
        part_starts.push(cur.read_i32::<LittleEndian>().map_err(io_err)? as usize);
    }

    let mut all_points = Vec::with_capacity(num_points);
    for _ in 0..num_points {
        let lon = cur.read_f64::<LittleEndian>().map_err(io_err)?;
        let lat = cur.read_f64::<LittleEndian>().map_err(io_err)?;
        all_points.push(Coordinate::new(lon, lat));
    }

    let mut parts: Vec<Vec<Coordinate>> = Vec::with_capacity(num_parts);
    for pi in 0..part_starts.len() {
        let start = part_starts[pi];
        let end = if pi + 1 < part_starts.len() {
            part_starts[pi + 1]
        } else {
            all_points.len()
        };
        if start <= end && end <= all_points.len() {
            parts.push(all_points[start..end].to_vec());
        }
    }

    let mut measures = MeasureArrays::default();
    match shape_type {
        SHP_POLYLINE_Z | SHP_POLYGON_Z => {
            measures.z = Some(read_measure_array(cur, num_points)?);
            measures.m = read_optional_measure_array(cur, num_points, content_end)?;
        }
        SHP_POLYLINE_M | SHP_POLYGON_M => {
            measures.m = Some(read_measure_array(cur, num_points)?);
        }
        _ => {}
    }

    let is_polygon = matches!(shape_type, SHP_POLYGON | SHP_POLYGON_Z | SHP_POLYGON_M);
    let geom = if is_polygon {
        Geometry::Polygon(parts)
    } else {
        Geometry::Polyline(parts)
    };
    Ok((geom, measures))
}

fn bytes_remaining(cur: &Cursor<&[u8]>, content_end: usize) -> usize {
    content_end.saturating_sub(cur.position() as usize)
}

/// Read `[min, max]` range then `n` f64 values (Z or M array).
fn read_measure_array(cur: &mut Cursor<&[u8]>, n: usize) -> Result<Vec<f64>, GeoError> {
    let _min = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let _max = cur.read_f64::<LittleEndian>().map_err(io_err)?;
    let mut vals = Vec::with_capacity(n);
    for _ in 0..n {
        vals.push(cur.read_f64::<LittleEndian>().map_err(io_err)?);
    }
    Ok(vals)
}

/// Attempt to read an optional trailing M array bounded by `content_end`.
fn read_optional_measure_array(
    cur: &mut Cursor<&[u8]>,
    n: usize,
    content_end: usize,
) -> Result<Option<Vec<f64>>, GeoError> {
    let need = 16 + 8 * n;
    if bytes_remaining(cur, content_end) < need {
        return Ok(None);
    }
    Ok(Some(read_measure_array(cur, n)?))
}

// ── .dbf parser (dBASE III+) ──────────────────────────────────────────────────

fn parse_dbf(data: &[u8]) -> Result<(Vec<FieldDef>, Vec<Vec<String>>), GeoError> {
    if data.len() < DBF_HEADER_SIZE {
        return Err(GeoError::InvalidFormat("DBF file too short".into()));
    }
    let mut cur = Cursor::new(data);

    let _version = cur.read_u8().map_err(io_err)?;
    cur.seek(SeekFrom::Current(3)).map_err(io_err)?; // date
    let num_records = cur.read_u32::<LittleEndian>().map_err(io_err)? as usize;
    let header_size = cur.read_u16::<LittleEndian>().map_err(io_err)? as usize;
    let record_size = cur.read_u16::<LittleEndian>().map_err(io_err)? as usize;
    cur.seek(SeekFrom::Current(20)).map_err(io_err)?; // reserved

    // Field descriptors
    let field_area = header_size.saturating_sub(DBF_HEADER_SIZE);
    let n_fields = field_area / DBF_FIELD_DESCRIPTOR_SIZE;
    let mut schema: Vec<FieldDef> = Vec::with_capacity(n_fields);

    for _ in 0..n_fields {
        let mut name_buf = [0u8; 11];
        cur.read_exact(&mut name_buf).map_err(io_err)?;
        let name = String::from_utf8_lossy(&name_buf)
            .trim_end_matches('\0')
            .to_string();
        let ft = cur.read_u8().map_err(io_err)? as char;
        cur.seek(SeekFrom::Current(4)).map_err(io_err)?; // reserved
        let length = cur.read_u8().map_err(io_err)?;
        let decimal_count = cur.read_u8().map_err(io_err)?;
        cur.seek(SeekFrom::Current(14)).map_err(io_err)?; // reserved

        // Detect terminator
        let pos = cur.position() as usize;
        if pos < data.len() && data[pos] == DBF_TERMINATOR {
            cur.seek(SeekFrom::Current(1)).map_err(io_err)?;
            schema.push(FieldDef {
                name,
                field_type: ft,
                length,
                decimal_count,
            });
            break;
        }
        schema.push(FieldDef {
            name,
            field_type: ft,
            length,
            decimal_count,
        });
    }

    // Seek to first record
    cur.seek(SeekFrom::Start(header_size as u64))
        .map_err(io_err)?;

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(num_records);
    for _ in 0..num_records {
        let delete_flag = cur.read_u8().map_err(io_err)?;
        if delete_flag == b'*' {
            // Deleted record — skip
            cur.seek(SeekFrom::Current((record_size - 1) as i64))
                .map_err(io_err)?;
            continue;
        }
        let mut row: Vec<String> = Vec::with_capacity(schema.len());
        for field in &schema {
            let mut buf = vec![0u8; field.length as usize];
            cur.read_exact(&mut buf).map_err(io_err)?;
            let val = String::from_utf8_lossy(&buf).trim().to_string();
            row.push(val);
        }
        rows.push(row);
    }

    Ok((schema, rows))
}

fn io_err(e: std::io::Error) -> GeoError {
    GeoError::Io(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_point_shp_roundtrip() {
        // Minimal SHP: file header + one Point record
        let mut shp = Vec::new();

        // File header (100 bytes)
        shp.extend_from_slice(&9994i32.to_be_bytes()); // file code
        shp.extend_from_slice(&[0u8; 20]); // unused
                                           // file length = (100 + 20) / 2 = 60 words
        shp.extend_from_slice(&60i32.to_be_bytes());
        shp.extend_from_slice(&1000i32.to_le_bytes()); // version
        shp.extend_from_slice(&1i32.to_le_bytes()); // shape type = Point
                                                    // Bounding box: min_x, min_y, max_x, max_y
        shp.extend_from_slice(&(-47.0f64).to_le_bytes());
        shp.extend_from_slice(&(-15.0f64).to_le_bytes());
        shp.extend_from_slice(&(-47.0f64).to_le_bytes());
        shp.extend_from_slice(&(-15.0f64).to_le_bytes());
        shp.extend_from_slice(&[0u8; 32]); // Z + M ranges
                                           // Padding to 100 bytes
        assert_eq!(shp.len(), 100);

        // Record header: record num = 1, content length = 10 words (20 bytes: shape_type + x + y)
        shp.extend_from_slice(&1i32.to_be_bytes());
        shp.extend_from_slice(&10i32.to_be_bytes());
        // Record content
        shp.extend_from_slice(&1i32.to_le_bytes()); // Point
        shp.extend_from_slice(&(-47.9172f64).to_le_bytes()); // lon (Brasília)
        shp.extend_from_slice(&(-15.7797f64).to_le_bytes()); // lat

        let (_bbox, records) = parse_shp(&shp).unwrap();
        assert_eq!(records.len(), 1);
        match &records[0].1 {
            Geometry::Point(p) => {
                assert!((p.lon - (-47.9172)).abs() < 1e-9);
                assert!((p.lat - (-15.7797)).abs() < 1e-9);
            }
            _ => panic!("Expected Point"),
        }
    }

    #[test]
    fn test_point_z_preserves_elevation() {
        // WHY: GIS elevation / SAR height attributes arrive as PointZ;
        // XY must parse and Z must surface on properties without inventing
        // a 3-D Geometry variant that would break WKT/Milvus.
        let mut shp = Vec::new();
        shp.extend_from_slice(&9994i32.to_be_bytes());
        shp.extend_from_slice(&[0u8; 20]);
        shp.extend_from_slice(&68i32.to_be_bytes()); // (100+36)/2
        shp.extend_from_slice(&1000i32.to_le_bytes());
        shp.extend_from_slice(&11i32.to_le_bytes()); // PointZ
        shp.extend_from_slice(&0.0f64.to_le_bytes());
        shp.extend_from_slice(&0.0f64.to_le_bytes());
        shp.extend_from_slice(&1.0f64.to_le_bytes());
        shp.extend_from_slice(&1.0f64.to_le_bytes());
        shp.extend_from_slice(&[0u8; 32]);
        assert_eq!(shp.len(), 100);

        // content: type + x + y + z + m = 4 + 32 = 36 bytes = 18 words
        shp.extend_from_slice(&1i32.to_be_bytes());
        shp.extend_from_slice(&18i32.to_be_bytes());
        shp.extend_from_slice(&11i32.to_le_bytes());
        shp.extend_from_slice(&(-47.9f64).to_le_bytes());
        shp.extend_from_slice(&(-15.8f64).to_le_bytes());
        shp.extend_from_slice(&(250.5f64).to_le_bytes()); // Z
        shp.extend_from_slice(&(f64::NAN).to_le_bytes()); // M unused

        let data = parse_shapefile_bytes(&shp, None).unwrap();
        assert_eq!(data.features.len(), 1);
        match &data.features[0].geometry {
            Geometry::Point(p) => {
                assert!((p.lon - (-47.9)).abs() < 1e-9);
                assert!((p.lat - (-15.8)).abs() < 1e-9);
            }
            _ => panic!("expected Point"),
        }
        let z = data.features[0]
            .get_property("_z")
            .and_then(|v| v.as_f64())
            .expect("_z");
        assert!((z - 250.5).abs() < 1e-9);
        assert!(data.features[0].get_property("_m").is_none());
    }
}
