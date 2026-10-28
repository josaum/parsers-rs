//! Thermal / infrared image parser.
//!
//! Reads:
//! - **FLIR R-JPEG**: radiometric JPEG with raw thermal data in APP1 EXIF segment
//! - **Thermal TIFF**: generic thermal TIFF (Seek, DJI Zenmuse XT2, etc.)
//!
//! Extracts: temperature matrix, min/max/avg temps, sensor metadata.

use crate::error::GeoError;

/// Planck constants used for radiometric → temperature conversion.
#[derive(Debug, Clone)]
pub struct PlanckConstants {
    /// R1: first Planck constant for radiometric conversion.
    pub r1: f64,
    /// R2: second Planck constant for radiometric conversion.
    pub r2: f64,
    /// B: Planck constant for radiometric conversion.
    pub b: f64,
    /// F: calibration factor for radiometric conversion.
    pub f: f64,
    /// O: offset constant for radiometric conversion.
    pub o: f64,
}

/// Default Planck constants for typical FLIR cameras (FLIR Lepton / Tau2 family).
impl Default for PlanckConstants {
    fn default() -> Self {
        PlanckConstants {
            r1: 21106.77,
            r2: 0.012545258,
            b: 1501.2,
            f: 1.0,
            o: -7340.0,
        }
    }
}

/// Metadata extracted from a thermal image.
#[derive(Debug, Clone)]
pub struct ThermalMetadata {
    /// Thermal raster width in pixels.
    pub width: u32,
    /// Thermal raster height in pixels.
    pub height: u32,
    /// Sensor or camera model identifier.
    pub sensor_model: String,
    /// Surface emissivity used for temperature conversion.
    pub emissivity: f32,
    /// Reflected apparent temperature in Celsius.
    pub reflected_temp_c: f32,
    /// Atmospheric temperature in Celsius.
    pub atmospheric_temp_c: f32,
    /// Camera-to-target distance in metres.
    pub distance_m: f32,
    /// Minimum temperature in Celsius.
    pub min_temp_c: f32,
    /// Maximum temperature in Celsius.
    pub max_temp_c: f32,
    /// Average temperature in Celsius.
    pub avg_temp_c: f32,
}

/// Parser for radiometric thermal JPEG/TIFF payloads.
pub struct ThermalParser;

impl ThermalParser {
    /// Parse a FLIR radiometric JPEG (R-JPEG).
    ///
    /// FLIR stores raw 16-bit radiometric data inside the JPEG file as an
    /// embedded segment.  The layout is:
    ///
    /// 1. Standard JPEG (visible thumbnail / radiometric overlay)
    /// 2. **FFF record** starting with magic `"FFF\0"` at a known offset
    ///    inside an APP1 or after the JPEG EOI marker.
    /// 3. Inside the FFF record: raw thermal data block + Planck constants.
    ///
    /// This parser locates the FFF segment, reads Planck constants, extracts
    /// the raw 16-bit array, and converts to temperatures.
    pub fn parse_flir_rjpeg(data: &[u8]) -> Result<(ThermalMetadata, Vec<f32>), GeoError> {
        // Locate FLIR FFF record — search for magic bytes "FFF\0"
        let fff_offset = Self::find_flir_fff(data)
            .ok_or_else(|| GeoError::InvalidFormat("FLIR FFF record not found in RJPEG".into()))?;

        let fff = &data[fff_offset..];
        if fff.len() < 64 {
            return Err(GeoError::InvalidFormat("FLIR FFF record too short".into()));
        }

        // FFF record header (simplified):
        //   0..4   : "FFF\0" magic
        //   4..8   : version
        //  32..36  : raw data offset (relative to FFF start)
        //  36..40  : raw data length in bytes
        //  40..44  : width
        //  44..48  : height

        let raw_offset = Self::read_le_u32(fff, 32) as usize;
        let raw_length = Self::read_le_u32(fff, 36) as usize;
        let width = Self::read_le_u32(fff, 40);
        let height = Self::read_le_u32(fff, 44);

        // Sanity fallback: if width/height are 0 try offsets 24..32
        let (width, height) = if width == 0 || height == 0 {
            let w = Self::read_le_u16(fff, 24) as u32;
            let h = Self::read_le_u16(fff, 26) as u32;
            if w == 0 || h == 0 {
                return Err(GeoError::InvalidFormat(
                    "Cannot determine image dimensions from FLIR FFF header".into(),
                ));
            }
            (w, h)
        } else {
            (width, height)
        };

        let pixel_count = (width * height) as usize;

        // Try to read Planck constants from the FFF record (offsets 48..88)
        let planck = if fff.len() >= 88 {
            PlanckConstants {
                r1: Self::read_le_f32(fff, 48) as f64,
                r2: Self::read_le_f32(fff, 52) as f64,
                b: Self::read_le_f32(fff, 56) as f64,
                f: Self::read_le_f32(fff, 60) as f64,
                o: Self::read_le_f32(fff, 64) as f64,
            }
        } else {
            PlanckConstants::default()
        };

        // Emissivity and other camera params (offsets 68..84)
        let emissivity = if fff.len() >= 72 {
            Self::read_le_f32(fff, 68)
        } else {
            0.95
        };
        let reflected_temp_c = if fff.len() >= 76 {
            Self::read_le_f32(fff, 72)
        } else {
            20.0
        };
        let atmospheric_temp_c = if fff.len() >= 80 {
            Self::read_le_f32(fff, 76)
        } else {
            20.0
        };
        let distance_m = if fff.len() >= 84 {
            Self::read_le_f32(fff, 80)
        } else {
            1.0
        };

        // Extract raw 16-bit radiometric data
        let raw_start = fff_offset + raw_offset;
        let raw_end = raw_start + raw_length;
        let expected_bytes = pixel_count * 2;

        let raw_slice = if (raw_end <= data.len() && raw_length >= expected_bytes)
            || fff_offset + raw_offset + expected_bytes <= data.len()
        {
            &data[raw_start..raw_start + expected_bytes]
        } else {
            return Err(GeoError::InvalidFormat(format!(
                "FLIR raw data out of bounds: need {} bytes at offset {}, file has {} bytes",
                expected_bytes,
                raw_start,
                data.len()
            )));
        };

        // Decode LE u16 values
        let raw_values: Vec<u16> = raw_slice
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();

        // Convert to temperature
        let temps = Self::raw_to_temperature(&raw_values, &planck, emissivity);

        let (min_t, max_t, avg_t) = Self::temp_stats(&temps);

        let meta = ThermalMetadata {
            width,
            height,
            sensor_model: "FLIR".into(),
            emissivity,
            reflected_temp_c,
            atmospheric_temp_c,
            distance_m,
            min_temp_c: min_t,
            max_temp_c: max_t,
            avg_temp_c: avg_t,
        };

        Ok((meta, temps))
    }

    /// Parse a thermal TIFF.
    ///
    /// Expects a single-band or dual-band TIFF where band 0 contains 16-bit
    /// raw radiometric values.  Common for Seek Thermal, DJI Zenmuse XT2.
    pub fn parse_thermal_tiff(data: &[u8]) -> Result<(ThermalMetadata, Vec<f32>), GeoError> {
        use std::io::Cursor;
        use tiff::decoder::Decoder;

        let cursor = Cursor::new(data);
        let mut decoder = Decoder::new(cursor)
            .map_err(|e| GeoError::Tiff(format!("Failed to open thermal TIFF: {e}")))?;

        let (width, height) = decoder
            .dimensions()
            .map_err(|e| GeoError::Tiff(format!("Failed to read TIFF dimensions: {e}")))?;

        let image = decoder
            .read_image()
            .map_err(|e| GeoError::Tiff(format!("Failed to decode TIFF image data: {e}")))?;

        let raw_values: Vec<u16> = match image {
            tiff::decoder::DecodingResult::U16(v) => v,
            tiff::decoder::DecodingResult::U8(v) => v.iter().map(|&b| b as u16).collect(),
            _ => {
                return Err(GeoError::UnsupportedFormat(
                    "Thermal TIFF must contain U8 or U16 data".into(),
                ));
            }
        };

        let pixel_count = (width * height) as usize;
        let raw_slice = if raw_values.len() >= pixel_count {
            &raw_values[..pixel_count]
        } else {
            return Err(GeoError::InvalidFormat(format!(
                "TIFF has {} pixels but only {} values decoded",
                pixel_count,
                raw_values.len()
            )));
        };

        let planck = PlanckConstants::default();
        let emissivity: f32 = 0.95;
        let temps = Self::raw_to_temperature(raw_slice, &planck, emissivity);
        let (min_t, max_t, avg_t) = Self::temp_stats(&temps);

        let meta = ThermalMetadata {
            width,
            height,
            sensor_model: "unknown".into(),
            emissivity,
            reflected_temp_c: 20.0,
            atmospheric_temp_c: 20.0,
            distance_m: 1.0,
            min_temp_c: min_t,
            max_temp_c: max_t,
            avg_temp_c: avg_t,
        };

        Ok((meta, temps))
    }

    /// Convert raw 16-bit radiometric values to temperature in Celsius.
    ///
    /// Uses Planck's law:
    /// ```text
    /// T = B / ln(R1 / (R2 * (S + O)) + F) - 273.15
    /// ```
    ///
    /// where `S` is the raw sensor value adjusted for emissivity.
    pub fn raw_to_temperature(raw: &[u16], planck: &PlanckConstants, emissivity: f32) -> Vec<f32> {
        let em = emissivity as f64;
        raw.iter()
            .map(|&s| {
                let s_adj = s as f64 / em;
                let denom = planck.r1 / (planck.r2 * (s_adj + planck.o)) + planck.f;
                if denom <= 0.0 {
                    return f32::NAN;
                }
                let t_kelvin = planck.b / denom.ln();
                (t_kelvin - 273.15) as f32
            })
            .collect()
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Scan for the FLIR "FFF\0" magic in the file.
    ///
    /// FLIR R-JPEG files embed the FFF record either inside an APP1 EXIF
    /// segment or after the JPEG EOI (0xFFD9) marker.
    fn find_flir_fff(data: &[u8]) -> Option<usize> {
        let magic = b"FFF\0";
        data.array_windows::<4>().position(|w| w == magic)
    }

    fn read_le_u32(data: &[u8], offset: usize) -> u32 {
        if offset + 4 > data.len() {
            return 0;
        }
        u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    fn read_le_u16(data: &[u8], offset: usize) -> u16 {
        if offset + 2 > data.len() {
            return 0;
        }
        u16::from_le_bytes([data[offset], data[offset + 1]])
    }

    fn read_le_f32(data: &[u8], offset: usize) -> f32 {
        if offset + 4 > data.len() {
            return 0.0;
        }
        f32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    fn temp_stats(temps: &[f32]) -> (f32, f32, f32) {
        if temps.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let mut min_t = f32::INFINITY;
        let mut max_t = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut count = 0u64;
        for &t in temps {
            if t.is_finite() {
                if t < min_t {
                    min_t = t;
                }
                if t > max_t {
                    max_t = t;
                }
                sum += t as f64;
                count += 1;
            }
        }
        let avg_t = if count > 0 {
            (sum / count as f64) as f32
        } else {
            0.0
        };
        (min_t, max_t, avg_t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_to_temperature_known_values() {
        let planck = PlanckConstants::default();
        // A mid-range raw value should produce a reasonable temperature
        let raw = vec![8000u16, 10000, 15000, 20000];
        let temps = ThermalParser::raw_to_temperature(&raw, &planck, 0.95);
        assert_eq!(temps.len(), 4);
        // All should be finite
        for t in &temps {
            assert!(t.is_finite(), "Temperature should be finite, got {t}");
        }
        // Higher raw values → higher temperatures (monotonic)
        for i in 1..temps.len() {
            assert!(
                temps[i] >= temps[i - 1],
                "Temperatures should increase with raw value: {} >= {}",
                temps[i],
                temps[i - 1]
            );
        }
    }

    #[test]
    fn test_temp_stats() {
        let temps = vec![20.0f32, 25.5, 30.0, 22.1];
        let (min_t, max_t, avg_t) = ThermalParser::temp_stats(&temps);
        assert!((min_t - 20.0).abs() < 1e-3);
        assert!((max_t - 30.0).abs() < 1e-3);
        assert!((avg_t - 24.4).abs() < 0.1);
    }

    #[test]
    fn test_find_flir_fff() {
        let mut data = vec![0xFFu8, 0xD8, 0xFF, 0xE0]; // JPEG SOI + APP0
        data.extend_from_slice(&[0; 100]); // padding
        data.extend_from_slice(b"FFF\0"); // FFF magic
        data.extend_from_slice(&[0; 200]); // rest

        let offset = ThermalParser::find_flir_fff(&data);
        assert_eq!(offset, Some(104));
    }
}
