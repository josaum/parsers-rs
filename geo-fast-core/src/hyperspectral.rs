//! Hyperspectral image parser.
//!
//! Reads **ENVI format** (`.hdr` + `.dat`) — the most common HSI interchange
//! format used by airborne and spaceborne imaging spectrometers.
//!
//! Extracts: band info, wavelength table, spatial metadata, interleave layout.

use crate::error::GeoError;
use crate::types::BBox;

/// ENVI data type codes.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum EnviDataType {
    /// Unsigned 8-bit integer.
    U8 = 1,
    /// Signed 16-bit integer.
    I16 = 2,
    /// Signed 32-bit integer.
    I32 = 3,
    /// 32-bit floating point.
    F32 = 4,
    /// 64-bit floating point.
    F64 = 5,
    /// Complex number with two 32-bit floats.
    ComplexF32 = 6,
    /// Complex number with two 64-bit floats.
    ComplexF64 = 9,
    /// Unsigned 16-bit integer.
    U16 = 12,
    /// Unsigned 32-bit integer.
    U32 = 13,
    /// Signed 64-bit integer.
    I64 = 14,
    /// Unsigned 64-bit integer.
    U64 = 15,
}

impl EnviDataType {
    /// Convert an ENVI `data type` numeric code into an [`EnviDataType`].
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::U8),
            2 => Some(Self::I16),
            3 => Some(Self::I32),
            4 => Some(Self::F32),
            5 => Some(Self::F64),
            6 => Some(Self::ComplexF32),
            9 => Some(Self::ComplexF64),
            12 => Some(Self::U16),
            13 => Some(Self::U32),
            14 => Some(Self::I64),
            15 => Some(Self::U64),
            _ => None,
        }
    }

    /// Size of a single element in bytes.
    pub fn byte_size(&self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 | Self::I64 | Self::U64 | Self::ComplexF32 => 8,
            Self::ComplexF64 => 16,
        }
    }
}

/// Interleave ordering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Interleave {
    /// Band Sequential: all pixels of band 0, then band 1, etc.
    Bsq,
    /// Band Interleaved by Line: line 0 of all bands, then line 1, etc.
    Bil,
    /// Band Interleaved by Pixel: pixel (0,0) all bands, pixel (1,0) all bands, etc.
    Bip,
}

impl Interleave {
    /// Parse an ENVI interleave token (`bsq`, `bil`, or `bip`).
    ///
    /// Named `from_token` (not `from_str`) so it does not shadow the
    /// [`std::str::FromStr`] trait method: this parse is infallible-optional
    /// (`Option`, no error payload) and case/whitespace-insensitive.
    pub fn from_token(s: &str) -> Option<Self> {
        match s.to_lowercase().trim() {
            "bsq" => Some(Self::Bsq),
            "bil" => Some(Self::Bil),
            "bip" => Some(Self::Bip),
            _ => None,
        }
    }

    /// Return the canonical uppercase ENVI interleave token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bsq => "BSQ",
            Self::Bil => "BIL",
            Self::Bip => "BIP",
        }
    }
}

/// Metadata extracted from an ENVI `.hdr` header file.
#[derive(Debug, Clone)]
pub struct HyperspectralMetadata {
    /// Number of lines (height / rows).
    pub lines: u32,
    /// Number of samples (width / columns).
    pub samples: u32,
    /// Number of spectral bands.
    pub bands: u32,
    /// ENVI data type code.
    pub data_type: EnviDataType,
    /// Interleave ordering.
    pub interleave: Interleave,
    /// Byte order: 0 = little-endian, 1 = big-endian.
    pub byte_order: u8,
    /// Header offset in bytes (skip before pixel data).
    pub header_offset: u64,
    /// Centre wavelength per band (nm or um).
    pub wavelengths: Vec<f64>,
    /// Wavelength units string ("Nanometers", "Micrometers", etc.).
    pub wavelength_units: String,
    /// Full width at half maximum per band.
    pub fwhm: Vec<f64>,
    /// Sensor type string.
    pub sensor_type: String,
    /// Description / title from the header.
    pub description: String,
    /// Geographic bounding box from `map info` if available.
    pub bbox: Option<BBox>,
}

/// Parser for ENVI hyperspectral headers and band data.
pub struct HyperspectralParser;

impl HyperspectralParser {
    /// Parse an ENVI `.hdr` header file.
    ///
    /// The ENVI header format is a simple `key = value` text file.
    /// Multi-value fields (wavelengths, fwhm) are enclosed in `{ }`.
    pub fn parse_envi_header(hdr_bytes: &[u8]) -> Result<HyperspectralMetadata, GeoError> {
        let text = std::str::from_utf8(hdr_bytes)
            .map_err(|e| GeoError::InvalidFormat(format!("ENVI header is not valid UTF-8: {e}")))?;

        // Check for "ENVI" magic on first line
        let first_line = text.lines().next().unwrap_or("");
        if !first_line.trim().starts_with("ENVI") {
            return Err(GeoError::InvalidFormat(
                "ENVI header must start with 'ENVI'".into(),
            ));
        }

        // Join continuation lines: if a line starts with whitespace and
        // the previous value had an unclosed `{`, it's a continuation.
        let joined = Self::join_continuation_lines(text);

        let mut lines: u32 = 0;
        let mut samples: u32 = 0;
        let mut bands: u32 = 0;
        let mut data_type_code: u8 = 4; // default F32
        let mut interleave_str = String::from("bsq");
        let mut byte_order: u8 = 0;
        let mut header_offset: u64 = 0;
        let mut wavelengths: Vec<f64> = Vec::new();
        let mut wavelength_units = String::new();
        let mut fwhm: Vec<f64> = Vec::new();
        let mut sensor_type = String::new();
        let mut description = String::new();
        let mut map_info_raw = String::new();

        for line in joined.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("ENVI") || line.starts_with(';') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().to_lowercase();
                let value = value.trim();

                match key.as_str() {
                    "lines" => lines = value.parse().unwrap_or(0),
                    "samples" => samples = value.parse().unwrap_or(0),
                    "bands" => bands = value.parse().unwrap_or(0),
                    "data type" => data_type_code = value.parse().unwrap_or(4),
                    "interleave" => interleave_str = value.to_string(),
                    "byte order" => byte_order = value.parse().unwrap_or(0),
                    "header offset" => header_offset = value.parse().unwrap_or(0),
                    "wavelength units" => wavelength_units = value.to_string(),
                    "sensor type" => sensor_type = value.to_string(),
                    "description" => description = Self::strip_braces(value),
                    "wavelength" => wavelengths = Self::parse_float_list(value),
                    "fwhm" => fwhm = Self::parse_float_list(value),
                    "map info" => map_info_raw = Self::strip_braces(value),
                    _ => {}
                }
            }
        }

        if lines == 0 || samples == 0 || bands == 0 {
            return Err(GeoError::InvalidFormat(
                "ENVI header missing required fields: lines, samples, bands".into(),
            ));
        }

        let data_type = EnviDataType::from_code(data_type_code).ok_or_else(|| {
            GeoError::UnsupportedFormat(format!("Unknown ENVI data type code: {data_type_code}"))
        })?;

        let interleave = Interleave::from_token(&interleave_str).ok_or_else(|| {
            GeoError::UnsupportedFormat(format!("Unknown interleave: '{interleave_str}'"))
        })?;

        let bbox = Self::parse_map_info(&map_info_raw, samples, lines);

        Ok(HyperspectralMetadata {
            lines,
            samples,
            bands,
            data_type,
            interleave,
            byte_order,
            header_offset,
            wavelengths,
            wavelength_units,
            fwhm,
            sensor_type,
            description,
            bbox,
        })
    }

    /// Read a single band from BSQ/BIL/BIP interleaved binary data.
    ///
    /// Returns pixel values as `Vec<f32>` (converted from the native type).
    pub fn read_band(
        data: &[u8],
        metadata: &HyperspectralMetadata,
        band_index: u32,
    ) -> Result<Vec<f32>, GeoError> {
        if band_index >= metadata.bands {
            return Err(GeoError::InvalidFormat(format!(
                "Band index {} out of range (0..{})",
                band_index, metadata.bands
            )));
        }

        let elem_size = metadata.data_type.byte_size();
        let pixels_per_band = (metadata.lines * metadata.samples) as usize;
        let offset = metadata.header_offset as usize;
        let big_endian = metadata.byte_order == 1;

        let data = &data[offset..];

        let mut result = Vec::with_capacity(pixels_per_band);

        match metadata.interleave {
            Interleave::Bsq => {
                // Band sequential: band_index * lines * samples * elem_size
                let band_start = band_index as usize * pixels_per_band * elem_size;
                for i in 0..pixels_per_band {
                    let pos = band_start + i * elem_size;
                    result.push(Self::read_pixel(
                        data,
                        pos,
                        &metadata.data_type,
                        big_endian,
                    )?);
                }
            }
            Interleave::Bil => {
                // Band interleaved by line
                let samples = metadata.samples as usize;
                let bands = metadata.bands as usize;
                let bi = band_index as usize;
                for line in 0..metadata.lines as usize {
                    let line_start = line * bands * samples * elem_size;
                    let band_start = line_start + bi * samples * elem_size;
                    for s in 0..samples {
                        let pos = band_start + s * elem_size;
                        result.push(Self::read_pixel(
                            data,
                            pos,
                            &metadata.data_type,
                            big_endian,
                        )?);
                    }
                }
            }
            Interleave::Bip => {
                // Band interleaved by pixel
                let bands = metadata.bands as usize;
                let bi = band_index as usize;
                for pixel in 0..pixels_per_band {
                    let pos = (pixel * bands + bi) * elem_size;
                    result.push(Self::read_pixel(
                        data,
                        pos,
                        &metadata.data_type,
                        big_endian,
                    )?);
                }
            }
        }

        Ok(result)
    }

    /// Compute NDVI from near-infrared and red bands.
    ///
    /// ```text
    /// NDVI = (NIR - Red) / (NIR + Red)
    /// ```
    ///
    /// Output is in `[-1.0, 1.0]`.  Pixels where `NIR + Red == 0` are set to 0.
    pub fn compute_ndvi(nir_band: &[f32], red_band: &[f32]) -> Vec<f32> {
        assert_eq!(
            nir_band.len(),
            red_band.len(),
            "NIR and Red bands must have equal length"
        );
        nir_band
            .iter()
            .zip(red_band.iter())
            .map(|(&nir, &red)| {
                let sum = nir + red;
                if sum.abs() < 1e-10 {
                    0.0
                } else {
                    (nir - red) / sum
                }
            })
            .collect()
    }

    /// Compute the spectral angle (in radians) between a pixel spectrum and
    /// a reference spectrum.
    ///
    /// ```text
    /// SAM = arccos( dot(a, b) / (|a| * |b|) )
    /// ```
    ///
    /// Smaller angle → more similar spectra.
    pub fn spectral_angle(pixel_spectrum: &[f32], reference: &[f32]) -> f32 {
        assert_eq!(
            pixel_spectrum.len(),
            reference.len(),
            "Spectra must have equal length"
        );
        let dot: f64 = pixel_spectrum
            .iter()
            .zip(reference.iter())
            .map(|(&a, &b)| a as f64 * b as f64)
            .sum();
        let norm_a: f64 = pixel_spectrum
            .iter()
            .map(|&a| (a as f64).powi(2))
            .sum::<f64>()
            .sqrt();
        let norm_b: f64 = reference
            .iter()
            .map(|&b| (b as f64).powi(2))
            .sum::<f64>()
            .sqrt();

        let denom = norm_a * norm_b;
        if denom < 1e-12 {
            return std::f32::consts::FRAC_PI_2; // 90 degrees for zero vectors
        }
        let cos_angle = (dot / denom).clamp(-1.0, 1.0);
        cos_angle.acos() as f32
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Join ENVI continuation lines (values wrapped inside `{ ... }`).
    fn join_continuation_lines(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut open_braces = 0i32;

        for line in text.lines() {
            if open_braces > 0 {
                // Continuation: append to previous line
                result.push(' ');
                result.push_str(line.trim());
            } else {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(line);
            }
            open_braces += line.matches('{').count() as i32;
            open_braces -= line.matches('}').count() as i32;
        }
        result
    }

    /// Strip surrounding `{ }` from a value string.
    fn strip_braces(s: &str) -> String {
        let s = s.trim();
        let s = s.strip_prefix('{').unwrap_or(s);
        let s = s.strip_suffix('}').unwrap_or(s);
        s.trim().to_string()
    }

    /// Parse a `{v1, v2, v3, ...}` float list from an ENVI value.
    fn parse_float_list(value: &str) -> Vec<f64> {
        let inner = Self::strip_braces(value);
        inner
            .split(',')
            .filter_map(|s| s.trim().parse::<f64>().ok())
            .collect()
    }

    /// Parse `map info` to extract a bounding box.
    ///
    /// ENVI map info format:
    /// `{projection, ref_x, ref_y, easting, northing, x_size, y_size, ...}`
    ///
    /// For UTM and Geographic, we can derive a rough BBox.
    fn parse_map_info(raw: &str, samples: u32, lines: u32) -> Option<BBox> {
        if raw.is_empty() {
            return None;
        }
        let parts: Vec<&str> = raw.split(',').map(|s| s.trim()).collect();
        if parts.len() < 7 {
            return None;
        }

        let _ref_x: f64 = parts[1].parse().ok()?;
        let _ref_y: f64 = parts[2].parse().ok()?;
        let easting: f64 = parts[3].parse().ok()?;
        let northing: f64 = parts[4].parse().ok()?;
        let x_size: f64 = parts[5].parse().ok()?;
        let y_size: f64 = parts[6].parse().ok()?;

        // For Geographic projection, easting/northing are lon/lat directly
        let projection = parts[0].to_lowercase();
        if projection.contains("geographic") {
            let min_lon = easting;
            let max_lat = northing;
            let max_lon = min_lon + x_size * samples as f64;
            let min_lat = max_lat - y_size * lines as f64;
            return Some(BBox::new(min_lon, min_lat, max_lon, max_lat));
        }

        // For UTM or other projections we'd need a full CRS transform,
        // which is out of scope.  Return None.
        None
    }

    /// Read a single pixel value from raw bytes at the given position.
    fn read_pixel(
        data: &[u8],
        pos: usize,
        data_type: &EnviDataType,
        big_endian: bool,
    ) -> Result<f32, GeoError> {
        let end = pos + data_type.byte_size();
        if end > data.len() {
            return Err(GeoError::InvalidFormat(format!(
                "Read past end of data at offset {pos} (data len {})",
                data.len()
            )));
        }
        let bytes = &data[pos..end];

        let val: f32 = match data_type {
            EnviDataType::U8 => bytes[0] as f32,
            EnviDataType::I16 => {
                let v = if big_endian {
                    i16::from_be_bytes([bytes[0], bytes[1]])
                } else {
                    i16::from_le_bytes([bytes[0], bytes[1]])
                };
                v as f32
            }
            EnviDataType::U16 => {
                let v = if big_endian {
                    u16::from_be_bytes([bytes[0], bytes[1]])
                } else {
                    u16::from_le_bytes([bytes[0], bytes[1]])
                };
                v as f32
            }
            EnviDataType::I32 => {
                let v = if big_endian {
                    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                } else {
                    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                };
                v as f32
            }
            EnviDataType::U32 => {
                let v = if big_endian {
                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                } else {
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                };
                v as f32
            }
            EnviDataType::F32 => {
                if big_endian {
                    f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                } else {
                    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                }
            }
            EnviDataType::F64 => {
                let v = if big_endian {
                    f64::from_be_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                } else {
                    f64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                };
                v as f32
            }
            EnviDataType::I64 => {
                let v = if big_endian {
                    i64::from_be_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                } else {
                    i64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                };
                v as f32
            }
            EnviDataType::U64 => {
                let v = if big_endian {
                    u64::from_be_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                } else {
                    u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ])
                };
                v as f32
            }
            EnviDataType::ComplexF32 | EnviDataType::ComplexF64 => {
                return Err(GeoError::UnsupportedFormat(format!(
                    "Reading {:?} pixel data not yet implemented",
                    data_type
                )));
            }
        };

        Ok(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_envi_header() -> Vec<u8> {
        b"ENVI\nsamples = 100\nlines = 50\nbands = 10\ndata type = 4\n\
          interleave = bsq\nbyte order = 0\nwavelength units = Nanometers\n\
          wavelength = { 450.0, 500.0, 550.0, 600.0, 650.0, 700.0, 750.0, 800.0, 850.0, 900.0 }\n\
          fwhm = { 10, 10, 10, 10, 10, 10, 10, 10, 10, 10 }\n\
          sensor type = Aviris\n\
          description = { Test hyperspectral image }\n"
            .to_vec()
    }

    #[test]
    fn test_parse_envi_header() {
        let hdr = sample_envi_header();
        let meta = HyperspectralParser::parse_envi_header(&hdr).unwrap();
        assert_eq!(meta.samples, 100);
        assert_eq!(meta.lines, 50);
        assert_eq!(meta.bands, 10);
        assert_eq!(meta.data_type, EnviDataType::F32);
        assert_eq!(meta.interleave, Interleave::Bsq);
        assert_eq!(meta.byte_order, 0);
        assert_eq!(meta.wavelengths.len(), 10);
        assert!((meta.wavelengths[0] - 450.0).abs() < 1e-6);
        assert!((meta.wavelengths[9] - 900.0).abs() < 1e-6);
        assert_eq!(meta.fwhm.len(), 10);
        assert_eq!(meta.sensor_type, "Aviris");
        assert_eq!(meta.description, "Test hyperspectral image");
    }

    #[test]
    fn test_read_band_bsq() {
        // Create a tiny BSQ dataset: 2x2 image, 3 bands, f32
        let samples: u32 = 2;
        let lines: u32 = 2;
        let bands: u32 = 3;
        let pixels = (samples * lines) as usize;

        let mut data = Vec::new();
        for band in 0..bands {
            for pixel in 0..pixels as u32 {
                let val = (band * 10 + pixel) as f32;
                data.extend_from_slice(&val.to_le_bytes());
            }
        }

        let meta = HyperspectralMetadata {
            lines,
            samples,
            bands,
            data_type: EnviDataType::F32,
            interleave: Interleave::Bsq,
            byte_order: 0,
            header_offset: 0,
            wavelengths: vec![500.0, 600.0, 700.0],
            wavelength_units: "Nanometers".into(),
            fwhm: vec![10.0, 10.0, 10.0],
            sensor_type: "Test".into(),
            description: String::new(),
            bbox: None,
        };

        let band0 = HyperspectralParser::read_band(&data, &meta, 0).unwrap();
        assert_eq!(band0, vec![0.0, 1.0, 2.0, 3.0]);

        let band2 = HyperspectralParser::read_band(&data, &meta, 2).unwrap();
        assert_eq!(band2, vec![20.0, 21.0, 22.0, 23.0]);
    }

    /// WHY: ENVI data types 14/15 (I64/U64) are in the enum and byte_size table
    /// but `read_pixel` still rejects them. Airborne/spaceborne cubes that ship
    /// 64-bit integer DN must round-trip through `read_band` as f32 without
    /// UnsupportedFormat — otherwise GIS HSI ingest fails loud on valid files.
    #[test]
    fn read_band_i64_and_u64_pixels() {
        let samples: u32 = 2;
        let lines: u32 = 1;
        let bands: u32 = 1;

        // I64 LE: [10, -20]
        let mut i64_data = Vec::new();
        i64_data.extend_from_slice(&10i64.to_le_bytes());
        i64_data.extend_from_slice(&(-20i64).to_le_bytes());
        let i64_meta = HyperspectralMetadata {
            lines,
            samples,
            bands,
            data_type: EnviDataType::I64,
            interleave: Interleave::Bsq,
            byte_order: 0,
            header_offset: 0,
            wavelengths: vec![500.0],
            wavelength_units: "Nanometers".into(),
            fwhm: vec![],
            sensor_type: "Test".into(),
            description: String::new(),
            bbox: None,
        };
        let i64_band = HyperspectralParser::read_band(&i64_data, &i64_meta, 0).unwrap();
        assert_eq!(i64_band, vec![10.0, -20.0]);

        // U64 LE: [3, 7]
        let mut u64_data = Vec::new();
        u64_data.extend_from_slice(&3u64.to_le_bytes());
        u64_data.extend_from_slice(&7u64.to_le_bytes());
        let u64_meta = HyperspectralMetadata {
            data_type: EnviDataType::U64,
            ..i64_meta
        };
        let u64_band = HyperspectralParser::read_band(&u64_data, &u64_meta, 0).unwrap();
        assert_eq!(u64_band, vec![3.0, 7.0]);
    }

    /// WHY: complex ENVI types (6/9) remain deferred — magnitude/phase policy
    /// is not defined for the f32 band path. They must fail loud, not silently
    /// take the first half of each complex pair as a real DN.
    #[test]
    fn read_band_complex_fails_loud() {
        let mut data = Vec::new();
        // One ComplexF32 sample: real=1.0, imag=2.0
        data.extend_from_slice(&1.0f32.to_le_bytes());
        data.extend_from_slice(&2.0f32.to_le_bytes());
        let meta = HyperspectralMetadata {
            lines: 1,
            samples: 1,
            bands: 1,
            data_type: EnviDataType::ComplexF32,
            interleave: Interleave::Bsq,
            byte_order: 0,
            header_offset: 0,
            wavelengths: vec![500.0],
            wavelength_units: "Nanometers".into(),
            fwhm: vec![],
            sensor_type: "Test".into(),
            description: String::new(),
            bbox: None,
        };
        let err = HyperspectralParser::read_band(&data, &meta, 0).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not yet implemented") || msg.contains("Complex"),
            "complex must fail loud; got: {msg}"
        );
    }

    #[test]
    fn test_compute_ndvi() {
        let nir = vec![0.8, 0.6, 0.0, 0.5];
        let red = vec![0.2, 0.4, 0.0, 0.5];
        let ndvi = HyperspectralParser::compute_ndvi(&nir, &red);
        assert!((ndvi[0] - 0.6).abs() < 1e-6); // (0.8-0.2)/(0.8+0.2)
        assert!((ndvi[1] - 0.2).abs() < 1e-6); // (0.6-0.4)/(0.6+0.4)
        assert!((ndvi[2] - 0.0).abs() < 1e-6); // zero / zero → 0
        assert!((ndvi[3] - 0.0).abs() < 1e-6); // equal → 0
    }

    #[test]
    fn test_spectral_angle() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let angle = HyperspectralParser::spectral_angle(&a, &b);
        // Orthogonal → pi/2
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-4);

        // Same direction → 0
        let c = vec![2.0, 0.0, 0.0];
        let angle2 = HyperspectralParser::spectral_angle(&a, &c);
        assert!(angle2.abs() < 1e-4);
    }

    #[test]
    fn test_parse_envi_header_with_map_info() {
        let hdr = b"ENVI\nsamples = 200\nlines = 100\nbands = 5\ndata type = 2\n\
                     interleave = bil\nbyte order = 0\n\
                     map info = { Geographic Lat/Lon, 1, 1, -60.0, -10.0, 0.001, 0.001 }\n";
        let meta = HyperspectralParser::parse_envi_header(hdr).unwrap();
        assert_eq!(meta.interleave, Interleave::Bil);
        let bbox = meta.bbox.unwrap();
        assert!((bbox.min_lon - (-60.0)).abs() < 1e-6);
        assert!((bbox.max_lat - (-10.0)).abs() < 1e-6);
    }
}
