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
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use tiff::decoder::{Decoder, DecodingResult, Limits};
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

/// `GTModelTypeGeoKey` value for a geographic (latitude-longitude) CRS.
///
/// The GeoTIFF specification defines 1 = projected, 2 = geographic,
/// 3 = geocentric.
const MODEL_TYPE_GEOGRAPHIC: u32 = 2;

/// TIFF `Compression` tag value for uncompressed data.
const COMPRESSION_NONE: u16 = 1;

/// Most decoded bytes one byte of an uncompressed TIFF can produce.
///
/// The decoder allocates the whole output raster from the width and height in
/// the header before it reads a strip, so those fields must be weighed against
/// the size of the input. Uncompressed samples are stored as they are decoded,
/// except that the decoder gives every sample at least one byte: packed 1-bit
/// samples grow eightfold.
const UNCOMPRESSED_EXPANSION: usize = 8;

/// Most decoded bytes one byte of a compressed TIFF can produce.
///
/// No codec the `tiff` crate decodes expands further than 4096:1. PackBits
/// reaches 64:1, Deflate 1032:1, JPEG about 1400:1 (one bit per block of a
/// DC-only scan), and LZW about 2560:1 (a 12-bit code naming a string of at
/// most 3838 bytes); 4096 is the next power of two. Sub-byte samples add the
/// factor of [`UNCOMPRESSED_EXPANSION`]. Lowering this rejects valid, highly
/// compressible rasters such as constant masks; raising it only lets a small
/// file claim a larger buffer.
const COMPRESSED_EXPANSION: usize = 4096 * UNCOMPRESSED_EXPANSION;

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
    let mut decoder = open_decoder(reader)?;

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
    // The tag holds (x, y, z) scales; one with fewer than two values gives no
    // usable scale and is treated like an absent tag.
    let pixel_scale =
        read_doubles_tag(&mut decoder, TAG_MODEL_PIXEL_SCALE)?.and_then(|v| match v.as_slice() {
            [x, y, ..] => Some((*x, *y)),
            _ => None,
        });

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
        .map(|v| v == MODEL_TYPE_GEOGRAPHIC)
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
    let mut decoder = open_decoder(BufReader::new(file))?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;
    let n_pixels = pixel_count(width, height)?;

    let result = decoder
        .read_image()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;

    match result {
        // Interleaved RGB(A) → split into bands
        DecodingResult::U8(data) => split_bands(&data, n_pixels, |v| f32::from(v) / 255.0),
        DecodingResult::U16(data) => split_bands(&data, n_pixels, |v| f32::from(v) / 65535.0),
        DecodingResult::F32(data) => {
            // Single band float
            Ok(vec![data])
        }
        DecodingResult::F64(data) => Ok(vec![data.iter().map(|&v| v as f32).collect()]),
        DecodingResult::I32(data) => split_bands(&data, n_pixels, |v| v as f32),
        DecodingResult::U32(data) => split_bands(&data, n_pixels, |v| v as f32),
        _ => Err(GeoError::UnsupportedFormat(
            "Unsupported pixel type in GeoTIFF".into(),
        )),
    }
}

/// Opens a TIFF decoder whose allocation limits follow the size of the input.
///
/// The `tiff` crate's default limits are constants (256 MiB of decoded
/// raster), so a file of a few hundred bytes could have the whole buffer
/// allocated from its header alone. Here the limits are also capped by what
/// `reader` can hold: see [`COMPRESSED_EXPANSION`].
pub(crate) fn open_decoder<R: Read + Seek>(mut reader: R) -> Result<Decoder<R>, GeoError> {
    let start = reader.stream_position().map_err(GeoError::Io)?;
    let input_len = reader.seek(SeekFrom::End(0)).map_err(GeoError::Io)?;
    reader.seek(SeekFrom::Start(start)).map_err(GeoError::Io)?;
    check_directory_fits(&mut reader, input_len)?;
    reader.seek(SeekFrom::Start(start)).map_err(GeoError::Io)?;
    let input_len = usize::try_from(input_len).unwrap_or(usize::MAX);

    let mut decoder = Decoder::new(reader).map_err(|e| GeoError::Tiff(e.to_string()))?;

    // Absent means uncompressed; a tag that cannot be read gets the looser bound.
    let uncompressed = matches!(
        decoder.find_tag_unsigned::<u16>(Tag::Compression),
        Ok(None | Some(COMPRESSION_NONE))
    );
    let expansion = if uncompressed {
        UNCOMPRESSED_EXPANSION
    } else {
        COMPRESSED_EXPANSION
    };
    let decoded = input_len.saturating_mul(expansion);

    let mut limits = Limits::default();
    limits.decoding_buffer_size = limits.decoding_buffer_size.min(decoded);
    limits.intermediate_buffer_size = limits.intermediate_buffer_size.min(decoded);
    // Tag values are stored as they are, so none is larger than the input.
    limits.ifd_value_size = limits.ifd_value_size.min(input_len);
    Ok(decoder.with_limits(limits))
}

/// Reads the fixed-width integers of a TIFF header and directory.
struct FieldReader<'a, R> {
    reader: &'a mut R,
    big_endian: bool,
}

impl<R: Read> FieldReader<'_, R> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], GeoError> {
        let mut bytes = [0u8; N];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| GeoError::Tiff(format!("TIFF header or directory is truncated: {e}")))?;
        Ok(bytes)
    }

    fn u16(&mut self) -> Result<u16, GeoError> {
        let bytes = self.bytes()?;
        Ok(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn u32(&mut self) -> Result<u32, GeoError> {
        let bytes = self.bytes()?;
        Ok(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    fn u64(&mut self) -> Result<u64, GeoError> {
        let bytes = self.bytes()?;
        Ok(if self.big_endian {
            u64::from_be_bytes(bytes)
        } else {
            u64::from_le_bytes(bytes)
        })
    }

    /// A directory offset or value count: 64-bit in BigTIFF, else 32-bit.
    fn word(&mut self, bigtiff: bool) -> Result<u64, GeoError> {
        if bigtiff {
            self.u64()
        } else {
            self.u32().map(u64::from)
        }
    }
}

/// Size in bytes of one value of a TIFF field type the decoder reads.
fn field_type_size(field_type: u16) -> Option<u64> {
    match field_type {
        // BYTE, ASCII, SBYTE, UNDEFINED
        1 | 2 | 6 | 7 => Some(1),
        // SHORT, SSHORT
        3 | 8 => Some(2),
        // LONG, SLONG, FLOAT, IFD
        4 | 9 | 11 | 13 => Some(4),
        // RATIONAL, SRATIONAL, DOUBLE, LONG8, SLONG8, IFD8
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

/// Rejects a TIFF whose first directory declares a tag value larger than the
/// file itself.
///
/// `Decoder::new` reads the first directory under the `tiff` crate's default
/// limits, and for each value it decodes it reserves memory from the entry's
/// declared count: a 200-byte file with one inflated count field is given up
/// to 256 MiB before any limit set through `with_limits` exists. A value is
/// stored in the file, so a count that needs more bytes than the file has is
/// malformed, and checking that first bounds those reservations by the input.
///
/// The walk mirrors the decoder's own: same header, same first directory,
/// same entry layout. Leaves `reader` at an unspecified position.
fn check_directory_fits<R: Read + Seek>(reader: &mut R, input_len: u64) -> Result<(), GeoError> {
    let mut fields = FieldReader {
        reader,
        big_endian: false,
    };
    fields.big_endian = match &fields.bytes::<2>()? {
        b"II" => false,
        b"MM" => true,
        // Not a TIFF; the decoder reports that.
        _ => return Ok(()),
    };
    let bigtiff = match fields.u16()? {
        42 => false,
        43 => {
            // Offset size and a reserved word; the decoder validates both.
            fields.u32()?;
            true
        }
        _ => return Ok(()),
    };

    let directory = fields.word(bigtiff)?;
    fields
        .reader
        .seek(SeekFrom::Start(directory))
        .map_err(GeoError::Io)?;
    let entries = if bigtiff {
        fields.u64()?
    } else {
        u64::from(fields.u16()?)
    };
    // Every pass reads at least twelve bytes or fails, so a count that
    // exceeds the file ends in the truncation error.
    for _ in 0..entries {
        let tag = fields.u16()?;
        let field_type = fields.u16()?;
        let Some(value_size) = field_type_size(field_type) else {
            // The decoder skips an entry of unknown type by reading eight
            // more bytes, in BigTIFF too; stay in step with it.
            fields.u64()?;
            continue;
        };
        let count = fields.word(bigtiff)?;
        let _value_or_offset = fields.word(bigtiff)?;
        if count
            .checked_mul(value_size)
            .is_none_or(|bytes| bytes > input_len)
        {
            return Err(GeoError::InvalidFormat(format!(
                "TIFF tag {tag} declares {count} values of {value_size} bytes each \
                 in a {input_len} byte file"
            )));
        }
    }
    Ok(())
}

/// Number of pixels in a `width`×`height` raster.
///
/// The product of two `u32` always fits in `u64`; it is an error only where
/// `usize` cannot hold it.
pub(crate) fn pixel_count(width: u32, height: u32) -> Result<usize, GeoError> {
    usize::try_from(u64::from(width) * u64::from(height)).map_err(|_| {
        GeoError::InvalidFormat(format!(
            "Raster dimensions {width}x{height} exceed addressable memory"
        ))
    })
}

/// Splits pixel-interleaved samples into one `Vec<f32>` per band.
///
/// The band count is the number of samples the decoder returned per pixel.
fn split_bands<T: Copy>(
    data: &[T],
    n_pixels: usize,
    to_f32: impl Fn(T) -> f32,
) -> Result<Vec<Vec<f32>>, GeoError> {
    let samples = if data.len() == n_pixels {
        1
    } else {
        data.len().checked_div(n_pixels).unwrap_or(0)
    };
    if samples == 0 {
        return Err(GeoError::InvalidFormat(format!(
            "TIFF decoded {} samples for {n_pixels} pixels",
            data.len()
        )));
    }
    // `samples * n_pixels <= data.len()`, so the bands together are no larger
    // than the decoded raster they are split from.
    let mut bands: Vec<Vec<f32>> = (0..samples).map(|_| Vec::with_capacity(n_pixels)).collect();
    for (i, &v) in data.iter().enumerate() {
        bands[i % samples].push(to_f32(v));
    }
    Ok(bands)
}

// ── Tag reading helpers ──────────────────────────────────────────────────────

/// The decoder's key for a tag number.
///
/// The `tiff` crate files the GeoTIFF tags under named variants, so a lookup
/// with `Tag::Unknown(number)` never finds them.
fn tag(tag_id: u16) -> Tag {
    Tag::from_u16_exhaustive(tag_id)
}

fn read_doubles_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<Vec<f64>>, GeoError> {
    match decoder.get_tag_f64_vec(tag(tag_id)) {
        Ok(v) => Ok(Some(v)),
        // Tag not present in this image — not an error
        Err(_) => Ok(None),
    }
}

fn read_ascii_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<String>, GeoError> {
    match decoder.get_tag_ascii_string(tag(tag_id)) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

fn read_geokey_directory<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    _double_params: &[f64],
    _ascii_params: &str,
) -> Result<HashMap<u16, u32>, GeoError> {
    let raw: Vec<u16> = match decoder.get_tag_u16_vec(tag(TAG_GEO_KEY_DIRECTORY)) {
        Ok(v) => v,
        // Tag absent — not a GeoTIFF, but still a valid TIFF
        Err(_) => return Ok(HashMap::new()),
    };

    // Directory header: [KeyDirectoryVersion, KeyRevision, MinorRevision, NumberOfKeys]
    // followed by one [KeyID, TIFFTagLocation, Count, ValueOffset] entry per key.
    let Some((header, entries)) = raw.split_at_checked(4) else {
        return Ok(HashMap::new());
    };
    // The directory cannot hold more keys than it has entries for.
    let n_keys = usize::from(header[3]).min(entries.len() / 4);
    let mut keys = HashMap::with_capacity(n_keys);

    for &[key_id, _tiff_tag_location, _count, value_offset] in
        entries.as_chunks::<4>().0.iter().take(n_keys)
    {
        // For SHORT values (TIFFTagLocation == 0), ValueOffset IS the value
        keys.insert(key_id, u32::from(value_offset));
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
