//! GeoTIFF parser.
//!
//! Reads georeferencing tags from a TIFF file:
//!   - ModelPixelScaleTag    (34264)
//!   - ModelTiepointTag      (33922)
//!   - ModelTransformationTag (34264 / 34264 — alias; actually 33920 for the full matrix)
//!   - GeoKeyDirectoryTag    (34735) + GeoDoubleParamsTag (34736) + GeoAsciiParamsTag (34737)
//!
//! Returns [`GeoTiffMetadata`] and, optionally, raster band data as `Vec<f32>`.

use std::collections::HashMap;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;
use tiff::ColorType;

use crate::error::GeoError;
use crate::types::BBox;

// ── GeoTIFF tag numbers (not exported by the `tiff` crate) ──────────────────
const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
const TAG_MODEL_TIEPOINT: u16 = 33922;
const TAG_MODEL_TRANSFORMATION: u16 = 34264;
const TAG_GEO_KEY_DIRECTORY: u16 = 34735;
const TAG_GEO_DOUBLE_PARAMS: u16 = 34736;
const TAG_GEO_ASCII_PARAMS: u16 = 34737;

// GeoKey IDs
const GEOKEY_MODEL_TYPE: u16 = 1024;
const GEOKEY_GEOGRAPHIC_TYPE: u16 = 2048;
const GEOKEY_PROJECTED_CS_TYPE: u16 = 3072;

/// Summary of georeferencing information extracted from a GeoTIFF.
#[derive(Debug, Clone)]
pub struct GeoTiffMetadata {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Number of bands (samples per pixel).
    pub band_count: u16,
    /// Geographic bounding box (EPSG:4326 when projection is known).
    pub bbox: Option<BBox>,
    /// Pixel size in projection units (x, y).
    pub pixel_scale: Option<(f64, f64)>,
    /// Tiepoint (pixel_x, pixel_y, pixel_z, world_x, world_y, world_z).
    pub tiepoints: Vec<[f64; 6]>,
    /// Full 4×4 affine transformation matrix (row-major), if present.
    pub transformation_matrix: Option<[f64; 16]>,
    /// EPSG code derived from the GeoKey directory.
    pub epsg_code: Option<u32>,
    /// Whether the CRS is geographic (true) or projected (false).
    pub is_geographic: bool,
    /// Raw GeoKey→value map.
    pub geokeys: HashMap<u16, u32>,
}

impl GeoTiffMetadata {
    /// Computes the upper-left world coordinate from tiepoints + pixel scale.
    pub fn upper_left(&self) -> Option<(f64, f64)> {
        let tp = self.tiepoints.first()?;
        // tp = [pixel_x, pixel_y, pixel_z, world_x, world_y, world_z]
        Some((tp[3], tp[4]))
    }

    /// Computes the lower-right world coordinate.
    pub fn lower_right(&self) -> Option<(f64, f64)> {
        let (world_x, world_y) = self.upper_left()?;
        let (sx, sy) = self.pixel_scale?;
        let lr_x = world_x + sx * self.width as f64;
        let lr_y = world_y - sy * self.height as f64;
        Some((lr_x, lr_y))
    }
}

/// Parse GeoTIFF metadata from a file path.
pub fn parse_geotiff_metadata<P: AsRef<Path>>(path: P) -> Result<GeoTiffMetadata, GeoError> {
    let file = std::fs::File::open(path.as_ref()).map_err(GeoError::Io)?;
    let reader = BufReader::new(file);
    parse_geotiff_metadata_reader(reader)
}

/// Parse GeoTIFF metadata from any `Read + Seek`.
pub fn parse_geotiff_metadata_reader<R: Read + Seek>(
    reader: R,
) -> Result<GeoTiffMetadata, GeoError> {
    let mut decoder = Decoder::new(reader).map_err(|e| GeoError::Tiff(e.to_string()))?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;

    let color_type = decoder
        .colortype()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;

    let band_count: u16 = match color_type {
        ColorType::Gray(_) => 1,
        ColorType::RGB(_) => 3,
        ColorType::RGBA(_) => 4,
        ColorType::CMYK(_) => 4,
        ColorType::YCbCr(_) => 3,
        _ => 1,
    };

    // ── Read raw GeoTIFF tags ────────────────────────────────────────────────
    let pixel_scale = read_doubles_tag(&mut decoder, TAG_MODEL_PIXEL_SCALE)?.map(|v| (v[0], v[1]));

    let raw_tiepoints = read_doubles_tag(&mut decoder, TAG_MODEL_TIEPOINT)?.unwrap_or_default();
    let tiepoints: Vec<[f64; 6]> = raw_tiepoints.as_chunks::<6>().0.to_vec();

    let transformation_matrix =
        read_doubles_tag(&mut decoder, TAG_MODEL_TRANSFORMATION)?.and_then(|v| {
            if v.len() >= 16 {
                let mut arr = [0f64; 16];
                arr.copy_from_slice(&v[..16]);
                Some(arr)
            } else {
                None
            }
        });

    // ── GeoKey directory ────────────────────────────────────────────────────
    let geo_double_params =
        read_doubles_tag(&mut decoder, TAG_GEO_DOUBLE_PARAMS)?.unwrap_or_default();
    let geo_ascii_params = read_ascii_tag(&mut decoder, TAG_GEO_ASCII_PARAMS)?.unwrap_or_default();

    let geokeys = read_geokey_directory(&mut decoder, &geo_double_params, &geo_ascii_params)?;

    let epsg_code = geokeys
        .get(&GEOKEY_PROJECTED_CS_TYPE)
        .or_else(|| geokeys.get(&GEOKEY_GEOGRAPHIC_TYPE))
        .copied()
        .filter(|&v| v > 0 && v != 32767);

    let is_geographic = geokeys
        .get(&GEOKEY_MODEL_TYPE)
        .copied()
        .map(|v| v == 1) // 1 = ModelTypeGeographic, 2 = ModelTypeProjected
        .unwrap_or(false);

    // ── Derive bounding box ─────────────────────────────────────────────────
    let bbox = derive_bbox(
        &tiepoints,
        pixel_scale,
        width,
        height,
        &transformation_matrix,
    );

    Ok(GeoTiffMetadata {
        width,
        height,
        band_count,
        bbox,
        pixel_scale,
        tiepoints,
        transformation_matrix,
        epsg_code,
        is_geographic,
        geokeys,
    })
}

/// Parse GeoTIFF and return raster data as one `Vec<f32>` per band.
///
/// For large images this can be expensive; callers should check metadata first.
pub fn parse_geotiff_bands<P: AsRef<Path>>(path: P) -> Result<Vec<Vec<f32>>, GeoError> {
    let file = std::fs::File::open(path.as_ref()).map_err(GeoError::Io)?;
    let reader = BufReader::new(file);
    let mut decoder = Decoder::new(reader).map_err(|e| GeoError::Tiff(e.to_string()))?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;
    let n_pixels = (width * height) as usize;

    let result = decoder
        .read_image()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;

    match result {
        DecodingResult::U8(data) => {
            // Interleaved RGB(A) → split into bands
            let samples = if data.len() == n_pixels {
                1
            } else {
                data.len() / n_pixels
            };
            let mut bands: Vec<Vec<f32>> =
                (0..samples).map(|_| Vec::with_capacity(n_pixels)).collect();
            for (i, &v) in data.iter().enumerate() {
                bands[i % samples].push(v as f32 / 255.0);
            }
            Ok(bands)
        }
        DecodingResult::U16(data) => {
            let samples = if data.len() == n_pixels {
                1
            } else {
                data.len() / n_pixels
            };
            let mut bands: Vec<Vec<f32>> =
                (0..samples).map(|_| Vec::with_capacity(n_pixels)).collect();
            for (i, &v) in data.iter().enumerate() {
                bands[i % samples].push(v as f32 / 65535.0);
            }
            Ok(bands)
        }
        DecodingResult::F32(data) => {
            // Single band float
            Ok(vec![data])
        }
        DecodingResult::F64(data) => Ok(vec![data.iter().map(|&v| v as f32).collect()]),
        DecodingResult::I32(data) => {
            let samples = if data.len() == n_pixels {
                1
            } else {
                data.len() / n_pixels
            };
            let mut bands: Vec<Vec<f32>> =
                (0..samples).map(|_| Vec::with_capacity(n_pixels)).collect();
            for (i, &v) in data.iter().enumerate() {
                bands[i % samples].push(v as f32);
            }
            Ok(bands)
        }
        DecodingResult::U32(data) => {
            let samples = if data.len() == n_pixels {
                1
            } else {
                data.len() / n_pixels
            };
            let mut bands: Vec<Vec<f32>> =
                (0..samples).map(|_| Vec::with_capacity(n_pixels)).collect();
            for (i, &v) in data.iter().enumerate() {
                bands[i % samples].push(v as f32);
            }
            Ok(bands)
        }
        _ => Err(GeoError::UnsupportedFormat(
            "Unsupported pixel type in GeoTIFF".into(),
        )),
    }
}

// ── Tag reading helpers ──────────────────────────────────────────────────────

fn read_doubles_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<Vec<f64>>, GeoError> {
    let tag = Tag::Unknown(tag_id);
    match decoder.get_tag_f64_vec(tag) {
        Ok(v) => Ok(Some(v)),
        // Tag not present in this image — not an error
        Err(_) => Ok(None),
    }
}

fn read_ascii_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<String>, GeoError> {
    let tag = Tag::Unknown(tag_id);
    match decoder.get_tag_ascii_string(tag) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

fn read_geokey_directory<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    _double_params: &[f64],
    _ascii_params: &str,
) -> Result<HashMap<u16, u32>, GeoError> {
    let tag = Tag::Unknown(TAG_GEO_KEY_DIRECTORY);
    let raw: Vec<u16> = match decoder.get_tag_u16_vec(tag) {
        Ok(v) => v,
        // Tag absent — not a GeoTIFF, but still a valid TIFF
        Err(_) => return Ok(HashMap::new()),
    };

    // Directory header: [KeyDirectoryVersion, KeyRevision, MinorRevision, NumberOfKeys]
    if raw.len() < 4 {
        return Ok(HashMap::new());
    }
    let n_keys = raw[3] as usize;
    let mut keys = HashMap::with_capacity(n_keys);

    for i in 0..n_keys {
        let base = 4 + i * 4;
        if base + 3 >= raw.len() {
            break;
        }
        let key_id = raw[base];
        let _tiff_tag_location = raw[base + 1]; // 0 = short, else double/ascii tag
        let _count = raw[base + 2];
        let value_offset = raw[base + 3];
        // For SHORT values (tiff_tag_location == 0), value_offset IS the value
        keys.insert(key_id, value_offset as u32);
    }
    Ok(keys)
}

fn derive_bbox(
    tiepoints: &[[f64; 6]],
    pixel_scale: Option<(f64, f64)>,
    width: u32,
    height: u32,
    transform: &Option<[f64; 16]>,
) -> Option<BBox> {
    // Try transformation matrix first
    if let Some(m) = transform {
        // Row-major 4×4 affine: world_x = m[0]*px + m[1]*py + m[3]
        //                        world_y = m[4]*px + m[5]*py + m[7]
        let ul_x = m[3];
        let ul_y = m[7];
        let lr_x = m[0] * width as f64 + m[1] * height as f64 + m[3];
        let lr_y = m[4] * width as f64 + m[5] * height as f64 + m[7];
        return Some(BBox::new(
            ul_x.min(lr_x),
            lr_y.min(ul_y),
            ul_x.max(lr_x),
            lr_y.max(ul_y),
        ));
    }

    // Fall back to tiepoint + pixel scale
    let tp = tiepoints.first()?;
    let (sx, sy) = pixel_scale?;
    let ul_x = tp[3] - tp[0] * sx;
    let ul_y = tp[4] + tp[1] * sy;
    let lr_x = ul_x + sx * width as f64;
    let lr_y = ul_y - sy * height as f64;

    Some(BBox::new(
        ul_x.min(lr_x),
        lr_y.min(ul_y),
        ul_x.max(lr_x),
        lr_y.max(ul_y),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bbox_derives_from_tiepoint_scale() {
        let tiepoints = vec![[0.0, 0.0, 0.0, -45.0, -10.0, 0.0]];
        let bbox = derive_bbox(&tiepoints, Some((0.001, 0.001)), 1000, 1000, &None).unwrap();
        assert!((bbox.min_lon - (-45.0)).abs() < 1e-9);
        assert!((bbox.max_lon - (-44.0)).abs() < 1e-9);
        assert!((bbox.min_lat - (-11.0)).abs() < 1e-9);
        assert!((bbox.max_lat - (-10.0)).abs() < 1e-9);
    }
}
