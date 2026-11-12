//! GeoTIFF parser.
//!
//! Reads georeferencing tags from a TIFF file:
//!   - ModelPixelScaleTag    (34264)
//!   - ModelTiepointTag      (33922)
//!   - ModelTransformationTag (34264 / 34264 — alias; actually 33920 for the full matrix)
//!   - GeoKeyDirectoryTag    (34735), for the keys whose value is stored in the
//!     directory itself; GeoDoubleParamsTag (34736) and GeoAsciiParamsTag
//!     (34737) are not read
//!
//! Returns [`GeoTiffMetadata`] and, optionally, raster band data as `Vec<f32>`.

use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use tiff::decoder::ifd::Value;
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use tiff::{ColorType, TiffError};

use crate::error::GeoError;
use crate::types::BBox;

// ── GeoTIFF tag numbers (not exported by the `tiff` crate) ──────────────────
const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
const TAG_MODEL_TIEPOINT: u16 = 33922;
const TAG_MODEL_TRANSFORMATION: u16 = 34264;
const TAG_GEO_KEY_DIRECTORY: u16 = 34735;

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
    /// Bounding box in degrees of longitude and latitude.
    ///
    /// Set only when the raster's CRS is geographic ([`Self::is_geographic`])
    /// and its tags locate it. The corners of a projected raster are in the
    /// units of its projection, usually metres; a [`BBox`] holds degrees and
    /// nothing here reprojects, so for a projected raster this is `None`.
    /// [`Self::upper_left`] and [`Self::lower_right`] give the corners in the
    /// raster's own CRS either way.
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
    /// Returns the world coordinate of the raster's upper-left corner.
    ///
    /// The coordinate is in the raster's own CRS and comes from the first
    /// tiepoint. A tiepoint ties a pixel to a world coordinate, and that
    /// pixel need not be the corner, so the pixel scale carries the
    /// coordinate back to pixel (0, 0). Returns `None` when there is no
    /// tiepoint, or when the tiepoint is not at the corner and there is no
    /// pixel scale to carry it there.
    pub fn upper_left(&self) -> Option<(f64, f64)> {
        tiepoint_origin(self.tiepoints.first()?, self.pixel_scale)
    }

    /// Returns the world coordinate of the raster's lower-right corner.
    ///
    /// One pixel scale per pixel away from [`Self::upper_left`], in the
    /// raster's own CRS. Returns `None` without a tiepoint and a pixel scale.
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
    let mut decoder = open_tag_decoder(reader)?;

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
    // Only keys stored in the directory itself are used. The double and
    // ASCII parameter tags hold the values of the other keys (citations,
    // ellipsoid axes), which nothing here reads; they are left alone so that
    // a citation in some legacy encoding cannot fail the parse.
    let geokeys = read_geokey_directory(&mut decoder)?;

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
    // A `BBox` is in degrees. Only a geographic raster has corners in
    // degrees; a projected one would put metres in the longitude and
    // latitude fields.
    let bbox = if is_geographic {
        derive_bbox(
            &tiepoints,
            pixel_scale,
            width,
            height,
            &transformation_matrix,
        )
    } else {
        None
    };

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
/// Unsigned 8- and 16-bit samples are scaled to `0.0..=1.0` by their full
/// range. Every other sample type (signed 8-, 16- and 32-bit integers,
/// unsigned 32-bit integers, 32- and 64-bit floats) keeps its value, rounded
/// to the nearest `f32` where `f32` cannot hold it exactly.
///
/// For large images this can be expensive; callers should check metadata first.
///
/// # Errors
///
/// Returns [`GeoError::Io`] when the file cannot be opened,
/// [`GeoError::Tiff`] or [`GeoError::InvalidFormat`] when it is not a TIFF
/// the decoder can read, and [`GeoError::UnsupportedFormat`] for 64-bit
/// integer samples, for a raster of several bands stored as separate
/// planes (`PlanarConfiguration` 2), and for a raster whose bands would take
/// more than 512 MiB as `f32` (134,217,728 samples across all bands).
pub fn parse_geotiff_bands<P: AsRef<Path>>(path: P) -> Result<Vec<Vec<f32>>, GeoError> {
    let file = std::fs::File::open(path.as_ref()).map_err(GeoError::Io)?;
    read_bands(BufReader::new(file), max_band_bytes())
}

/// Bytes of band data returned per byte of raster the decoder may decode.
///
/// The decoder caps the raster it decodes (`Limits::decoding_buffer_size`,
/// 256 MiB by default), but the bands are `f32` whatever the samples were,
/// so an 8-bit raster at that cap would become four times as large again: a
/// file of a few hundred kilobytes of constant pixels asked for 1 GiB. Two
/// is the factor of a 16-bit raster, so every 16-, 32- and 64-bit raster the
/// decoder accepts still fits, among them a 10980×10980 16-bit satellite
/// band. An 8-bit raster fits up to half the decoder's cap, 128 MiB of
/// samples.
const BAND_BYTES_PER_DECODED_BYTE: usize = 2;

/// Most bytes of `f32` band data [`parse_geotiff_bands`] returns.
///
/// [`BAND_BYTES_PER_DECODED_BYTE`] times the decoder's own raster cap: 512
/// MiB, or 134,217,728 samples across all bands.
fn max_band_bytes() -> usize {
    Limits::default()
        .decoding_buffer_size
        .saturating_mul(BAND_BYTES_PER_DECODED_BYTE)
}

/// Checks that the bands of a raster fit in `max_band_bytes` as `f32`.
///
/// # Errors
///
/// Returns [`GeoError::UnsupportedFormat`] when `width` × `height` × `bands`
/// samples of four bytes each exceed `max_band_bytes` or overflow.
fn check_band_bytes(
    width: u32,
    height: u32,
    bands: u16,
    max_band_bytes: usize,
) -> Result<(), GeoError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(u64::from(bands)))
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>() as u64));
    let limit = u64::try_from(max_band_bytes).unwrap_or(u64::MAX);
    match bytes {
        Some(bytes) if bytes <= limit => Ok(()),
        _ => Err(GeoError::UnsupportedFormat(format!(
            "GeoTIFF raster of {width}x{height} pixels in {bands} bands needs more than \
             the {max_band_bytes} bytes of band data this reader returns"
        ))),
    }
}

/// Reads the bands of a TIFF, returning at most `max_band_bytes` of them.
///
/// The size is checked from the header, before anything is decoded.
fn read_bands<R: Read + Seek>(reader: R, max_band_bytes: usize) -> Result<Vec<Vec<f32>>, GeoError> {
    let mut decoder = open_decoder(reader)?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;
    let n_pixels = pixel_count(width, height)?;
    let bands = band_count(&mut decoder)?;
    check_band_bytes(width, height, bands, max_band_bytes)?;

    let result = decoder
        .read_image()
        .map_err(|e| GeoError::Tiff(e.to_string()))?;

    match result {
        // Interleaved RGB(A) → split into bands
        DecodingResult::U8(data) => split_bands(&data, n_pixels, |v| f32::from(v) / 255.0),
        DecodingResult::U16(data) => split_bands(&data, n_pixels, |v| f32::from(v) / 65535.0),
        DecodingResult::F32(data) => {
            if data.len() == n_pixels {
                // One band: the decoded raster is the band.
                Ok(vec![data])
            } else {
                split_bands(&data, n_pixels, |v| v)
            }
        }
        DecodingResult::F64(data) => split_bands(&data, n_pixels, |v| v as f32),
        // Signed and 32-bit samples keep their values: they are measurements
        // (an elevation in metres, a count), not intensities with a full
        // scale to normalise by.
        DecodingResult::I8(data) => split_bands(&data, n_pixels, f32::from),
        DecodingResult::I16(data) => split_bands(&data, n_pixels, f32::from),
        DecodingResult::I32(data) => split_bands(&data, n_pixels, |v| v as f32),
        DecodingResult::U32(data) => split_bands(&data, n_pixels, |v| v as f32),
        DecodingResult::U64(_) | DecodingResult::I64(_) => Err(GeoError::UnsupportedFormat(
            "Unsupported pixel type in GeoTIFF: 64-bit integer samples".into(),
        )),
    }
}

/// TIFF `PlanarConfiguration` tag value for one plane of samples per band.
///
/// The other value, 1 and the default, interleaves the samples of a pixel.
const PLANAR_CONFIGURATION_SEPARATE: u16 = 2;

/// Number of bands the raster's samples will be split into.
///
/// This is the `SamplesPerPixel` tag, 1 when absent.
///
/// # Errors
///
/// Returns [`GeoError::UnsupportedFormat`] for a raster of more than one
/// band stored a plane per band: the decoder reads the first plane only, and
/// handing that back would drop the other bands without a word. Returns
/// [`GeoError::Tiff`] when either tag cannot be read.
fn band_count<R: Read + Seek>(decoder: &mut Decoder<R>) -> Result<u16, GeoError> {
    let samples = decoder
        .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)
        .map_err(|e| GeoError::Tiff(e.to_string()))?
        .unwrap_or(1);
    let planar_configuration = decoder
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .map_err(|e| GeoError::Tiff(e.to_string()))?;
    if samples > 1 && planar_configuration == Some(PLANAR_CONFIGURATION_SEPARATE) {
        return Err(GeoError::UnsupportedFormat(format!(
            "GeoTIFF stores its {samples} bands as separate planes \
             (PlanarConfiguration 2); only pixel-interleaved bands are read"
        )));
    }
    Ok(samples)
}

/// Opens a TIFF decoder whose allocation limits follow the size of the input.
///
/// The `tiff` crate's default limits are constants (256 MiB of decoded
/// raster), so a file of a few hundred bytes could have the whole buffer
/// allocated from its header alone. Here the limits are also capped by what
/// `reader` can hold: see [`COMPRESSED_EXPANSION`].
///
/// These limits are sized for decoding the raster. The decoder applies the
/// same field to the arrays it builds for tag values, where it is too tight:
/// a caller that reads tags and no pixels uses [`open_tag_decoder`].
pub(crate) fn open_decoder<R: Read + Seek>(reader: R) -> Result<Decoder<R>, GeoError> {
    let (mut decoder, input_len) = open_checked(reader)?;

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

/// Opens a TIFF decoder for reading tag values, not pixels.
///
/// The decoder turns a tag's array into one [`Value`] per element and
/// refuses an array of more than `decoding_buffer_size / size_of::<Value>()`
/// elements. An element takes at least one byte of the file, so a tag that
/// fits in the file has at most `input_len` elements: allowing
/// `size_of::<Value>()` bytes per input byte admits every such tag, and still
/// ties the allocation to the size of the input. The raster limits of
/// [`open_decoder`] allow 8 bytes per input byte of an uncompressed file,
/// which is a quarter of that: under them a GeoKey directory that took up
/// more than a quarter of a small file's bytes could not be read.
fn open_tag_decoder<R: Read + Seek>(reader: R) -> Result<Decoder<R>, GeoError> {
    let (decoder, input_len) = open_checked(reader)?;

    let mut limits = Limits::default();
    limits.decoding_buffer_size = limits
        .decoding_buffer_size
        .min(input_len.saturating_mul(std::mem::size_of::<Value>()));
    // No strip or tile is decoded through this decoder.
    limits.intermediate_buffer_size = limits.intermediate_buffer_size.min(input_len);
    limits.ifd_value_size = limits.ifd_value_size.min(input_len);
    Ok(decoder.with_limits(limits))
}

/// Opens a TIFF decoder after checking its first directory against the file.
///
/// Returns the decoder, still under the `tiff` crate's default limits, and
/// the length of the input in bytes.
fn open_checked<R: Read + Seek>(mut reader: R) -> Result<(Decoder<R>, usize), GeoError> {
    let start = reader.stream_position().map_err(GeoError::Io)?;
    let input_len = reader.seek(SeekFrom::End(0)).map_err(GeoError::Io)?;
    reader.seek(SeekFrom::Start(start)).map_err(GeoError::Io)?;
    check_directory_fits(&mut reader, input_len)?;
    reader.seek(SeekFrom::Start(start)).map_err(GeoError::Io)?;
    let input_len = usize::try_from(input_len).unwrap_or(usize::MAX);

    let decoder = Decoder::new(reader).map_err(|e| GeoError::Tiff(e.to_string()))?;
    Ok((decoder, input_len))
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

/// Reads the value of an optional GeoTIFF tag.
///
/// Returns `Ok(None)` only when the image has no such tag. A tag that is
/// present but cannot be read (its data lies outside the file, or exceeds
/// the decoder's limits) is an error: reporting it as absent would hand back
/// a raster that silently lost its georeferencing.
fn find_geo_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<Value>, GeoError> {
    decoder
        .find_tag(tag(tag_id))
        .map_err(|e| unreadable_tag(tag_id, &e))
}

fn unreadable_tag(tag_id: u16, error: &TiffError) -> GeoError {
    GeoError::Tiff(format!("GeoTIFF tag {tag_id} cannot be read: {error}"))
}

/// Reads an optional GeoTIFF tag whose values are doubles.
///
/// # Errors
///
/// Returns [`GeoError::Tiff`] when the tag is present but unreadable, or
/// holds values of another type. A missing tag is `Ok(None)`.
fn read_doubles_tag<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    tag_id: u16,
) -> Result<Option<Vec<f64>>, GeoError> {
    find_geo_tag(decoder, tag_id)?
        .map(|value| value.into_f64_vec().map_err(|e| unreadable_tag(tag_id, &e)))
        .transpose()
}

/// Reads the GeoKey directory into a key→value map.
///
/// # Errors
///
/// Returns [`GeoError::Tiff`] when the directory tag is present but
/// unreadable, or is not an array of unsigned integers. A TIFF without the
/// tag is not a GeoTIFF and yields an empty map.
fn read_geokey_directory<R: Read + Seek>(
    decoder: &mut Decoder<R>,
) -> Result<HashMap<u16, u32>, GeoError> {
    // Tag absent: not a GeoTIFF, but still a valid TIFF.
    let Some(value) = find_geo_tag(decoder, TAG_GEO_KEY_DIRECTORY)? else {
        return Ok(HashMap::new());
    };
    let raw: Vec<u16> = value
        .into_u16_vec()
        .map_err(|e| unreadable_tag(TAG_GEO_KEY_DIRECTORY, &e))?;

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

/// World coordinate of pixel (0, 0), from one tiepoint and the pixel scale.
///
/// `tiepoint` is `[I, J, K, X, Y, Z]`: raster position (`I`, `J`) lies at
/// world position (`X`, `Y`). World Y decreases as `J` grows. A tiepoint at
/// the corner itself needs no scale; any other one cannot be carried to the
/// corner without it.
fn tiepoint_origin(tiepoint: &[f64; 6], pixel_scale: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let [i, j, _, x, y, _] = *tiepoint;
    if i == 0.0 && j == 0.0 {
        return Some((x, y));
    }
    let (sx, sy) = pixel_scale?;
    Some((x - i * sx, y + j * sy))
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
        // The matrix may rotate or shear the raster, and then the extremes
        // are not at the upper-left and lower-right corners: take all four.
        let (w, h) = (f64::from(width), f64::from(height));
        let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)]
            .map(|(px, py)| (m[0] * px + m[1] * py + m[3], m[4] * px + m[5] * py + m[7]));
        let (first_x, first_y) = corners[0];
        let mut bbox = BBox::new(first_x, first_y, first_x, first_y);
        for (x, y) in corners {
            bbox.min_lon = bbox.min_lon.min(x);
            bbox.max_lon = bbox.max_lon.max(x);
            bbox.min_lat = bbox.min_lat.min(y);
            bbox.max_lat = bbox.max_lat.max(y);
        }
        return Some(bbox);
    }

    // Fall back to tiepoint + pixel scale
    let (sx, sy) = pixel_scale?;
    let (ul_x, ul_y) = tiepoint_origin(tiepoints.first()?, pixel_scale)?;
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
    use std::io::Cursor;

    use tiff::encoder::{colortype, TiffEncoder};

    use super::*;

    /// GeoKey directory of a raster in UTM zone 33N (EPSG:32633), projected.
    const UTM_33N_KEYS: [u16; 16] = [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 32633];
    /// GeoKey directory of a raster in WGS 84 (EPSG:4326), geographic.
    const WGS84_KEYS: [u16; 16] = [1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326];

    /// The georeferencing tags of a test GeoTIFF; `None` leaves a tag out.
    #[derive(Default)]
    struct GeoTags<'a> {
        pixel_scale: Option<[f64; 3]>,
        tiepoint: Option<[f64; 6]>,
        transformation: Option<[f64; 16]>,
        geokeys: Option<&'a [u16]>,
    }

    /// Encodes a `width`×`height` 8-bit gray GeoTIFF carrying `tags`.
    fn geotiff(width: u32, height: u32, tags: &GeoTags<'_>) -> Vec<u8> {
        let mut file = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut file).expect("encoder");
            let mut image = encoder
                .new_image::<colortype::Gray8>(width, height)
                .expect("image");
            let directory = image.encoder();
            if let Some(scale) = &tags.pixel_scale {
                directory
                    .write_tag(Tag::ModelPixelScaleTag, &scale[..])
                    .expect("pixel scale");
            }
            if let Some(tiepoint) = &tags.tiepoint {
                directory
                    .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
                    .expect("tiepoint");
            }
            if let Some(matrix) = &tags.transformation {
                directory
                    .write_tag(Tag::ModelTransformationTag, &matrix[..])
                    .expect("transformation");
            }
            if let Some(geokeys) = tags.geokeys {
                directory
                    .write_tag(Tag::GeoKeyDirectoryTag, geokeys)
                    .expect("geokeys");
            }
            let pixels = vec![0u8; pixel_count(width, height).expect("pixel count")];
            image.write_data(&pixels).expect("pixels");
        }
        file.into_inner()
    }

    fn metadata(bytes: Vec<u8>) -> GeoTiffMetadata {
        parse_geotiff_metadata_reader(Cursor::new(bytes)).expect("GeoTIFF metadata")
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    fn assert_bbox(bbox: &BBox, min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) {
        assert_close(bbox.min_lon, min_lon);
        assert_close(bbox.min_lat, min_lat);
        assert_close(bbox.max_lon, max_lon);
        assert_close(bbox.max_lat, max_lat);
    }

    /// A UTM raster used to report `min_lon: 500000.0, min_lat: 4648276.0`:
    /// metres in fields that hold degrees.
    #[test]
    fn projected_raster_has_no_degree_bbox() {
        let projected = metadata(geotiff(
            100,
            50,
            &GeoTags {
                pixel_scale: Some([30.0, 30.0, 0.0]),
                tiepoint: Some([0.0, 0.0, 0.0, 500_000.0, 4_649_776.0, 0.0]),
                geokeys: Some(&UTM_33N_KEYS),
                ..GeoTags::default()
            },
        ));
        assert_eq!(projected.epsg_code, Some(32633));
        assert!(!projected.is_geographic);
        assert_eq!(projected.bbox, None);
        // The corners are still available, in the projection's own units.
        assert_eq!(projected.upper_left(), Some((500_000.0, 4_649_776.0)));
        assert_eq!(projected.lower_right(), Some((503_000.0, 4_648_276.0)));
    }

    #[test]
    fn geographic_raster_keeps_its_degree_bbox() {
        let geographic = metadata(geotiff(
            100,
            50,
            &GeoTags {
                pixel_scale: Some([0.01, 0.01, 0.0]),
                tiepoint: Some([0.0, 0.0, 0.0, -48.0, -15.0, 0.0]),
                geokeys: Some(&WGS84_KEYS),
                ..GeoTags::default()
            },
        ));
        assert_eq!(geographic.epsg_code, Some(4326));
        assert!(geographic.is_geographic);
        let bbox = geographic.bbox.expect("bbox of a geographic raster");
        assert_bbox(&bbox, -48.0, -15.5, -47.0, -15.0);
    }

    /// `upper_left` returned the tiepoint's world coordinate as it stood, so
    /// a raster tied at its centre reported its centre as its corner, and
    /// disagreed with `bbox`, which did account for the tiepoint's pixel.
    #[test]
    fn corners_account_for_a_tiepoint_away_from_the_origin() {
        let centred = metadata(geotiff(
            100,
            50,
            &GeoTags {
                pixel_scale: Some([0.01, 0.01, 0.0]),
                tiepoint: Some([50.0, 25.0, 0.0, -47.5, -15.25, 0.0]),
                geokeys: Some(&WGS84_KEYS),
                ..GeoTags::default()
            },
        ));
        let (ul_x, ul_y) = centred.upper_left().expect("upper left");
        let (lr_x, lr_y) = centred.lower_right().expect("lower right");
        assert_close(ul_x, -48.0);
        assert_close(ul_y, -15.0);
        assert_close(lr_x, -47.0);
        assert_close(lr_y, -15.5);
        // The corners and the bbox describe the same rectangle.
        let bbox = centred.bbox.expect("bbox of a geographic raster");
        assert_bbox(&bbox, ul_x, lr_y, lr_x, ul_y);
    }

    #[test]
    fn corner_of_an_off_origin_tiepoint_needs_the_pixel_scale() {
        let at_origin = [0.0, 0.0, 0.0, -48.0, -15.0, 0.0];
        assert_eq!(tiepoint_origin(&at_origin, None), Some((-48.0, -15.0)));
        let off_origin = [50.0, 25.0, 0.0, -47.5, -15.25, 0.0];
        assert_eq!(tiepoint_origin(&off_origin, None), None);
    }

    /// A 4×4 `ModelTransformation` that scales by `scale` and rotates by
    /// `degrees`, with pixel (0, 0) at (`x`, `y`).
    fn rotation(degrees: f64, scale: f64, x: f64, y: f64) -> [f64; 16] {
        let (sin, cos) = degrees.to_radians().sin_cos();
        let (c, s) = (cos * scale, sin * scale);
        [
            c, s, 0.0, x, s, -c, 0.0, y, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]
    }

    /// Only the upper-left and lower-right corners were used. On a raster
    /// rotated by 30° the other two are the northernmost and southernmost
    /// points, so the box was too short: 4649776..4649843 for a raster that
    /// spans 4649343..4650276.
    #[test]
    fn rotated_transformation_bbox_covers_all_four_corners() {
        let matrix = rotation(30.0, 10.0, 500_000.0, 4_649_776.0);
        let (c, s) = (matrix[0], matrix[1]);
        let bbox = derive_bbox(&[], None, 100, 50, &Some(matrix)).expect("bbox");
        // Corners: UL (0, 0), UR (100, 0), LL (0, 50), LR (100, 50).
        assert_bbox(
            &bbox,
            500_000.0,
            4_649_776.0 - 50.0 * c,
            500_000.0 + 100.0 * c + 50.0 * s,
            4_649_776.0 + 100.0 * s,
        );
        assert_close(bbox.min_lat, 4_649_342.987_298_108);
        assert_close(bbox.max_lat, 4_650_276.0);
    }

    #[test]
    fn rotated_geographic_raster_reports_its_full_extent() {
        let rotated = metadata(geotiff(
            100,
            50,
            &GeoTags {
                transformation: Some(rotation(30.0, 0.01, -48.0, -15.0)),
                geokeys: Some(&WGS84_KEYS),
                ..GeoTags::default()
            },
        ));
        let bbox = rotated.bbox.expect("bbox of a geographic raster");
        let (c, s) = (30f64.to_radians().cos() * 0.01, 0.005);
        assert_bbox(
            &bbox,
            -48.0,
            -15.0 - 50.0 * c,
            -48.0 + 100.0 * c + 50.0 * s,
            -15.0 + 100.0 * s,
        );
    }

    /// An axis-aligned matrix is the common case and is unchanged.
    #[test]
    fn unrotated_transformation_bbox_is_unchanged() {
        let matrix = rotation(0.0, 0.01, -48.0, -15.0);
        let bbox = derive_bbox(&[], None, 100, 50, &Some(matrix)).expect("bbox");
        assert_bbox(&bbox, -48.0, -15.5, -47.0, -15.0);
    }

    /// A GeoKey directory of `extra` keys beyond the model type and the
    /// geographic CRS, for a file whose directory outweighs its pixels.
    ///
    /// The extra keys take IDs from the private range (32768 and up), which
    /// no key this parser interprets falls in.
    fn long_geokey_directory(extra: u16) -> Vec<u16> {
        let mut keys = vec![1, 1, 0, extra + 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        for index in 0..extra {
            keys.extend_from_slice(&[32768 + index, 0, 1, 1]);
        }
        keys
    }

    /// Tag arrays were read under the raster's limit of 8 bytes per input
    /// byte, which the decoder divides by its 32-byte value type: a
    /// directory of more than a quarter of the file's bytes was refused, and
    /// the refusal was then taken for a missing tag. A 642-byte file with 42
    /// keys came back with no keys, no EPSG code and `is_geographic: false`.
    #[test]
    fn geokey_directory_larger_than_the_pixels_is_read_in_full() {
        // Measured before the fix on 2×2 files: 30 extra keys (132 values,
        // 562 bytes) were read and 40 extra keys (172 values, 642 bytes)
        // were lost. 4000 keys make the directory nearly the whole file.
        for extra in [8u16, 30, 40, 60, 500, 4000] {
            let keys = long_geokey_directory(extra);
            let parsed = metadata(geotiff(
                2,
                2,
                &GeoTags {
                    pixel_scale: Some([0.01, 0.01, 0.0]),
                    tiepoint: Some([0.0, 0.0, 0.0, -48.0, -15.0, 0.0]),
                    geokeys: Some(&keys),
                    ..GeoTags::default()
                },
            ));
            assert_eq!(parsed.geokeys.len(), usize::from(extra) + 2, "{extra}");
            assert_eq!(parsed.epsg_code, Some(4326), "{extra}");
            assert!(parsed.is_geographic, "{extra}");
            assert!(parsed.bbox.is_some(), "{extra}");
        }
    }

    /// Any error from reading a georeferencing tag used to mean "no such
    /// tag", so a damaged tag produced a raster without georeferencing and
    /// no error. Only a tag that is not there may do that.
    #[test]
    fn unreadable_georeferencing_tag_is_an_error_not_a_missing_tag() {
        // A pixel scale written as text, not as doubles.
        let mut file = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut file).expect("encoder");
            let mut image = encoder.new_image::<colortype::Gray8>(2, 2).expect("image");
            let directory = image.encoder();
            directory
                .write_tag(Tag::ModelPixelScaleTag, "0.01 0.01 0")
                .expect("pixel scale");
            directory
                .write_tag(Tag::GeoKeyDirectoryTag, &WGS84_KEYS[..])
                .expect("geokeys");
            image.write_data(&[0u8; 4]).expect("pixels");
        }
        let error = parse_geotiff_metadata_reader(Cursor::new(file.into_inner()))
            .expect_err("a mistyped pixel scale is an error");
        assert!(
            matches!(&error, GeoError::Tiff(message) if message.contains("33550")),
            "{error}"
        );

        // A GeoKey directory of doubles, not of unsigned integers.
        let mut file = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut file).expect("encoder");
            let mut image = encoder.new_image::<colortype::Gray8>(2, 2).expect("image");
            image
                .encoder()
                .write_tag(Tag::GeoKeyDirectoryTag, &[1.0f64, 1.0, 0.0, 0.0][..])
                .expect("geokeys");
            image.write_data(&[0u8; 4]).expect("pixels");
        }
        let error = parse_geotiff_metadata_reader(Cursor::new(file.into_inner()))
            .expect_err("a mistyped GeoKey directory is an error");
        assert!(
            matches!(&error, GeoError::Tiff(message) if message.contains("34735")),
            "{error}"
        );
    }

    /// A TIFF with no georeferencing tags at all is still a valid TIFF.
    #[test]
    fn missing_georeferencing_tags_are_not_an_error() {
        let plain = metadata(geotiff(2, 2, &GeoTags::default()));
        assert_eq!(plain.pixel_scale, None);
        assert!(plain.tiepoints.is_empty());
        assert_eq!(plain.transformation_matrix, None);
        assert!(plain.geokeys.is_empty());
        assert_eq!(plain.epsg_code, None);
        assert_eq!(plain.bbox, None);
    }

    /// A citation that is not UTF-8 lives in a tag this parser does not
    /// read, so it cannot fail the parse.
    #[test]
    fn non_utf8_citation_does_not_fail_the_parse() {
        let mut file = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut file).expect("encoder");
            let mut image = encoder.new_image::<colortype::Gray8>(2, 2).expect("image");
            let directory = image.encoder();
            directory
                .write_tag(Tag::GeoKeyDirectoryTag, &WGS84_KEYS[..])
                .expect("geokeys");
            // "Córrego Alegre|" in Latin-1, as BYTE values under the ASCII
            // parameters tag's number.
            directory
                .write_tag(Tag::GeoAsciiParamsTag, &b"C\xF3rrego Alegre|"[..])
                .expect("citation");
            image.write_data(&[0u8; 4]).expect("pixels");
        }
        let parsed = metadata(file.into_inner());
        assert_eq!(parsed.epsg_code, Some(4326));
    }

    /// Without a GeoKey directory nothing says the coordinates are degrees.
    #[test]
    fn raster_without_a_model_type_has_no_degree_bbox() {
        let untyped = metadata(geotiff(
            100,
            50,
            &GeoTags {
                pixel_scale: Some([0.01, 0.01, 0.0]),
                tiepoint: Some([0.0, 0.0, 0.0, -48.0, -15.0, 0.0]),
                ..GeoTags::default()
            },
        ));
        assert!(!untyped.is_geographic);
        assert_eq!(untyped.bbox, None);
    }

    #[test]
    fn test_bbox_derives_from_tiepoint_scale() {
        let tiepoints = vec![[0.0, 0.0, 0.0, -45.0, -10.0, 0.0]];
        let bbox = derive_bbox(&tiepoints, Some((0.001, 0.001)), 1000, 1000, &None).unwrap();
        assert!((bbox.min_lon - (-45.0)).abs() < 1e-9);
        assert!((bbox.max_lon - (-44.0)).abs() < 1e-9);
        assert!((bbox.min_lat - (-11.0)).abs() < 1e-9);
        assert!((bbox.max_lat - (-10.0)).abs() < 1e-9);
    }

    /// Encodes a `width`×`height` raster of colour type `C` holding `pixels`.
    fn raster<C>(width: u32, height: u32, pixels: &[C::Inner]) -> Vec<u8>
    where
        C: colortype::ColorType,
        [C::Inner]: tiff::encoder::TiffValue,
    {
        let mut file = Cursor::new(Vec::new());
        TiffEncoder::new(&mut file)
            .expect("encoder")
            .write_image::<C>(width, height, pixels)
            .expect("image");
        file.into_inner()
    }

    /// Reads the bands of an encoded raster through the public, by-path API.
    fn bands_of(bytes: &[u8]) -> Result<Vec<Vec<f32>>, GeoError> {
        let file = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(file.path(), bytes).expect("write raster");
        parse_geotiff_bands(file.path())
    }

    /// Signed 8- and 16-bit rasters were refused with "Unsupported pixel type
    /// in GeoTIFF". Elevation models are commonly 16-bit signed.
    #[test]
    fn signed_integer_rasters_are_read_as_their_values() {
        let elevation = [-32768i16, -120, 0, 8848];
        assert_eq!(
            bands_of(&raster::<colortype::GrayI16>(2, 2, &elevation)).expect("i16"),
            vec![vec![-32768.0, -120.0, 0.0, 8848.0]]
        );
        assert_eq!(
            bands_of(&raster::<colortype::GrayI8>(2, 2, &[-128i8, -1, 0, 127])).expect("i8"),
            vec![vec![-128.0, -1.0, 0.0, 127.0]]
        );
    }

    /// 32-bit integers already had arms; they are pinned here beside the
    /// narrower types they now sit with.
    #[test]
    fn wide_integer_rasters_are_read_as_their_values() {
        assert_eq!(
            bands_of(&raster::<colortype::GrayI32>(2, 1, &[-70_000i32, 70_000])).expect("i32"),
            vec![vec![-70_000.0, 70_000.0]]
        );
        assert_eq!(
            bands_of(&raster::<colortype::Gray32>(2, 1, &[0u32, 3_000_000_000])).expect("u32"),
            vec![vec![0.0, 3_000_000_000.0]]
        );
    }

    /// A three-band float raster came back as one "band" of all twelve
    /// interleaved samples.
    #[test]
    fn multi_band_float_raster_is_split_into_its_bands() {
        let pixels = [
            1.0f32, 10.0, 100.0, 2.0, 20.0, 200.0, 3.0, 30.0, 300.0, 4.0, 40.0, 400.0,
        ];
        assert_eq!(
            bands_of(&raster::<colortype::RGB32Float>(2, 2, &pixels)).expect("f32"),
            vec![
                vec![1.0, 2.0, 3.0, 4.0],
                vec![10.0, 20.0, 30.0, 40.0],
                vec![100.0, 200.0, 300.0, 400.0],
            ]
        );
        let pixels = [1.0f64, 10.0, 100.0, 2.0, 20.0, 200.0];
        assert_eq!(
            bands_of(&raster::<colortype::RGB64Float>(2, 1, &pixels)).expect("f64"),
            vec![vec![1.0, 2.0], vec![10.0, 20.0], vec![100.0, 200.0]]
        );
        // One band stays one band.
        assert_eq!(
            bands_of(&raster::<colortype::Gray32Float>(2, 1, &[0.5f32, -0.5])).expect("gray"),
            vec![vec![0.5, -0.5]]
        );
    }

    /// A 2×2 8-bit RGB TIFF stored one plane per band (`PlanarConfiguration`
    /// 2): three strips of four samples. The encoder cannot write one.
    fn planar_rgb() -> Vec<u8> {
        const SHORT: u16 = 3;
        const LONG: u16 = 4;
        let mut file = Vec::new();
        file.extend_from_slice(b"II");
        file.extend_from_slice(&42u16.to_le_bytes());
        file.extend_from_slice(&50u32.to_le_bytes()); // directory offset
        file.extend_from_slice(&[1, 2, 3, 4, 10, 20, 30, 40, 100, 110, 120, 130]); // at 8
        for bits in [8u16; 3] {
            file.extend_from_slice(&bits.to_le_bytes()); // at 20
        }
        for strip_offset in [8u32, 12, 16] {
            file.extend_from_slice(&strip_offset.to_le_bytes()); // at 26
        }
        for strip_bytes in [4u32; 3] {
            file.extend_from_slice(&strip_bytes.to_le_bytes()); // at 38
        }
        // (tag, type, count, value or offset), in tag order.
        let entries: [(u16, u16, u32, u32); 10] = [
            (256, SHORT, 1, 2),  // ImageWidth
            (257, SHORT, 1, 2),  // ImageLength
            (258, SHORT, 3, 20), // BitsPerSample
            (259, SHORT, 1, 1),  // Compression: none
            (262, SHORT, 1, 2),  // PhotometricInterpretation: RGB
            (273, LONG, 3, 26),  // StripOffsets
            (277, SHORT, 1, 3),  // SamplesPerPixel
            (278, SHORT, 1, 2),  // RowsPerStrip
            (279, LONG, 3, 38),  // StripByteCounts
            (284, SHORT, 1, 2),  // PlanarConfiguration: separate planes
        ];
        file.extend_from_slice(&10u16.to_le_bytes());
        for (tag, field_type, count, value) in entries {
            file.extend_from_slice(&tag.to_le_bytes());
            file.extend_from_slice(&field_type.to_le_bytes());
            file.extend_from_slice(&count.to_le_bytes());
            file.extend_from_slice(&value.to_le_bytes());
        }
        file.extend_from_slice(&0u32.to_le_bytes()); // no next directory
        file
    }

    /// Of a raster stored one plane per band the decoder reads the first
    /// plane only, so a three-band raster came back as a single band with
    /// nothing to say two were missing.
    #[test]
    fn planar_multi_band_raster_is_refused_not_cut_to_one_band() {
        let bytes = planar_rgb();
        // What the decoder hands back: the four samples of the first plane.
        let decoded = open_decoder(Cursor::new(bytes.clone()))
            .expect("decoder")
            .read_image()
            .expect("first plane");
        assert!(matches!(decoded, DecodingResult::U8(plane) if plane == [1, 2, 3, 4]));

        let error = bands_of(&bytes).expect_err("planar RGB");
        assert!(
            matches!(&error, GeoError::UnsupportedFormat(message) if message.contains("3 bands")),
            "{error}"
        );
    }

    /// The decoder caps the raster it decodes at 256 MiB, but nothing capped
    /// the `f32` bands made from it: a small, highly compressed 8-bit file
    /// decoding to that cap went on to allocate four times as much.
    #[test]
    fn bands_larger_than_the_output_limit_are_refused_before_decoding() {
        // 4×4 RGB: 48 samples, 192 bytes as f32.
        let rgb = raster::<colortype::RGB8>(4, 4, &[7u8; 48]);
        let bands = read_bands(Cursor::new(rgb.clone()), 192).expect("at the limit");
        assert_eq!(bands.len(), 3);
        assert!(bands.iter().all(|band| band.len() == 16));
        let error = read_bands(Cursor::new(rgb), 191).expect_err("one byte over");
        assert!(
            matches!(&error, GeoError::UnsupportedFormat(message)
                if message.contains("4x4") && message.contains("3 bands") && message.contains("191")),
            "{error}"
        );
        // A float raster is as large decoded as returned, and is held to
        // the same limit.
        let float = raster::<colortype::Gray32Float>(2, 2, &[0.5f32; 4]);
        assert!(read_bands(Cursor::new(float.clone()), 16).is_ok());
        assert!(read_bands(Cursor::new(float), 15).is_err());
    }

    #[test]
    fn output_limit_admits_every_16_bit_raster_the_decoder_accepts() {
        let limit = max_band_bytes();
        assert_eq!(limit, 512 * 1024 * 1024);
        // A 10980×10980 16-bit satellite band: 230 MiB decoded, 460 MiB out.
        assert!(check_band_bytes(10_980, 10_980, 1, limit).is_ok());
        // The largest single band, and one row more.
        assert!(check_band_bytes(16_384, 8_192, 1, limit).is_ok());
        assert!(check_band_bytes(16_384, 8_193, 1, limit).is_err());
        // The bands count: three of 8192×8192 are 768 MiB.
        assert!(check_band_bytes(8_192, 8_192, 2, limit).is_ok());
        assert!(check_band_bytes(8_192, 8_192, 3, limit).is_err());
        // Dimensions whose product overflows are refused, not wrapped.
        assert!(check_band_bytes(u32::MAX, u32::MAX, u16::MAX, usize::MAX).is_err());
    }
}
