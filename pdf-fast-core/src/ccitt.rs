// Rust guideline compliant 2026-02-21
use crate::PdfImageInfo;
use anyhow::{anyhow, Result};
use hayro_ccitt::{DecodeSettings, Decoder, DecoderContext, EncodingMode};
use image::ImageEncoder;

/// Largest image, in pixels, the CCITT decoder allocates an output buffer for.
///
/// One byte per pixel, so this caps the luma buffer at 100 MB. Checked before
/// any allocation so a hostile `/Width` and `/Height` are refused up front.
const MAX_PIXELS: u64 = 100_000_000;

/// Luma for pixels the stream does not code: columns past `/Columns` and rows
/// after the last decoded row.
///
/// hayro-ccitt 0.2 left `/BlackIs1` inversion to the decoder, and this module
/// padded with the post-inversion white value, so padding is always `0xFF`
/// regardless of `/BlackIs1`. Kept so decoded output does not change.
const PADDING: u8 = 0xFF;

pub(crate) struct CcittLumaImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

pub(crate) fn decode_ccitt_to_png(raw: &[u8], image: &PdfImageInfo) -> Result<Vec<u8>> {
    let luma = decode_ccitt_to_luma(raw, image)?;
    let CcittLumaImage {
        width,
        height,
        pixels,
    } = luma;
    let mut png = Vec::with_capacity(pixels.len().saturating_div(8).max(1024));
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, width, height, image::ExtendedColorType::L8)
        .map_err(|error| anyhow!("encode CCITT image as PNG: {error}"))?;
    Ok(png)
}

pub(crate) fn decode_ccitt_to_luma(raw: &[u8], image: &PdfImageInfo) -> Result<CcittLumaImage> {
    let width = positive_u32(image.width, "Width")?;
    let height = positive_u32(image.height, "Height")?;
    let params = CcittParams::from_image(image);
    let columns = params.columns.unwrap_or(width);
    let rows = params.rows.unwrap_or(height);
    let settings = DecodeSettings {
        columns,
        rows,
        end_of_block: params.end_of_block,
        end_of_line: params.end_of_line,
        rows_are_byte_aligned: params.encoded_byte_align,
        // `/K` only selects the scheme: hayro-ccitt reads each Group 3 2D
        // row's tag bit, so the period between 1D rows is not needed.
        encoding: if params.k < 0 {
            EncodingMode::Group4
        } else if params.k == 0 {
            EncodingMode::Group3_1D
        } else {
            EncodingMode::Group3_2D
        },
        // Recover from damaged data as well as possible, as 0.2 did.
        strict: false,
    };

    // The sink checks the pixel budget before anything is allocated.
    let mut sink = GraySink::new(width, height, columns, params.black_is_1)?;
    let mut context = DecoderContext::new(settings);
    let result = hayro_ccitt::decode(raw, &mut sink, &mut context);
    if let Err(error) = result {
        // Rows pushed before the error are complete; keep them and pad.
        if sink.decoded_rows == 0 {
            return Err(anyhow!("CCITT decode failed: {error}"));
        }
    }

    let pixels = sink.finish();
    Ok(CcittLumaImage {
        width,
        height,
        pixels,
    })
}

fn positive_u32(value: Option<i64>, key: &str) -> Result<u32> {
    let value = value.ok_or_else(|| anyhow!("CCITT image missing /{key}"))?;
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow!("CCITT image has invalid /{key}: {value}"))
}

#[derive(Debug)]
struct CcittParams {
    k: i64,
    columns: Option<u32>,
    rows: Option<u32>,
    end_of_block: bool,
    end_of_line: bool,
    encoded_byte_align: bool,
    black_is_1: bool,
}

impl CcittParams {
    fn from_image(image: &PdfImageInfo) -> Self {
        let dict = image.decode_parms.as_ref();
        Self {
            k: dict.and_then(|dict| dict.get_integer("K")).unwrap_or(0),
            columns: dict
                .and_then(|dict| dict.get_integer("Columns"))
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0),
            rows: dict
                .and_then(|dict| dict.get_integer("Rows"))
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0),
            end_of_block: dict
                .and_then(|dict| dict.get_bool("EndOfBlock"))
                .unwrap_or(true),
            end_of_line: dict
                .and_then(|dict| dict.get_bool("EndOfLine"))
                .unwrap_or(false),
            encoded_byte_align: dict
                .and_then(|dict| dict.get_bool("EncodedByteAlign"))
                .unwrap_or(false),
            black_is_1: dict
                .and_then(|dict| dict.get_bool("BlackIs1"))
                .unwrap_or(false),
        }
    }
}

/// Collects decoded rows into a `width` x `height` 8-bit luma buffer.
struct GraySink {
    width: u32,
    height: u32,
    /// Columns the stream codes per row, clipped to `width`.
    coded_columns: u32,
    decoded_rows: u32,
    /// Luma for pixels coded white, after `/BlackIs1`.
    white: u8,
    /// Luma for pixels coded black, after `/BlackIs1`.
    black: u8,
    pixels: Vec<u8>,
}

impl GraySink {
    fn new(width: u32, height: u32, columns: u32, black_is_1: bool) -> Result<Self> {
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| anyhow!("CCITT image dimensions overflow"))?;
        if pixels > MAX_PIXELS {
            return Err(anyhow!("CCITT image exceeds pixel budget"));
        }
        let len =
            usize::try_from(pixels).map_err(|_| anyhow!("CCITT image dimensions overflow"))?;
        // `/BlackIs1 true` means 1 bits are black, so the coded colors swap
        // relative to the default DeviceGray reading of the decoded bits.
        let (white, black) = if black_is_1 {
            (0x00, 0xFF)
        } else {
            (0xFF, 0x00)
        };
        Ok(Self {
            width,
            height,
            coded_columns: columns.min(width),
            decoded_rows: 0,
            white,
            black,
            pixels: Vec::with_capacity(len),
        })
    }

    fn finish(mut self) -> Vec<u8> {
        let expected = (self.width as usize).saturating_mul(self.height as usize);
        // Within the capacity reserved in `new`, so this never reallocates.
        self.pixels.resize(expected, PADDING);
        self.pixels
    }
}

impl Decoder for GraySink {
    fn push_row(&mut self, changes: &[u32]) {
        if self.decoded_rows >= self.height {
            return;
        }
        let width = self.width as usize;
        let coded = self.coded_columns as usize;
        let start = self.pixels.len();
        // Fewer than `height` rows are pushed here, so this stays within the
        // capacity reserved in `new`.
        self.pixels.resize(start.saturating_add(width), PADDING);
        if let Some(row) = self.pixels.get_mut(start..start.saturating_add(coded)) {
            row.fill(self.white);
            // Changes alternate white-to-black and black-to-white; an odd count
            // means the row ends black.
            for span in changes.chunks(2) {
                let Some(&from) = span.first() else {
                    continue;
                };
                let from = (from as usize).min(coded);
                let to = span.get(1).map_or(coded, |&to| (to as usize).min(coded));
                if let Some(run) = row.get_mut(from..to) {
                    run.fill(self.black);
                }
            }
        }
        self.decoded_rows += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PdfDictionary, PdfDictionaryEntry, PdfValue};

    const W: u8 = 0xFF;
    const B: u8 = 0x00;

    fn image(width: i64, height: i64, parms: &[(&str, PdfValue)]) -> PdfImageInfo {
        let entries = parms
            .iter()
            .map(|(key, value)| PdfDictionaryEntry {
                key: (*key).to_string(),
                value: value.clone(),
            })
            .collect();
        PdfImageInfo {
            width: Some(width),
            height: Some(height),
            bits_per_component: Some(1),
            color_space: Some("DeviceGray".to_string()),
            format_hint: None,
            decode_parms: Some(PdfDictionary { entries }),
        }
    }

    fn decode(raw: &[u8], image: &PdfImageInfo) -> Vec<u8> {
        let luma = decode_ccitt_to_luma(raw, image).expect("decode CCITT test vector");
        assert_eq!(
            luma.pixels.len(),
            (luma.width as usize) * (luma.height as usize)
        );
        luma.pixels
    }

    /// Group 3 1D, two 8-pixel rows of 4 white then 4 black (`1011` `011`).
    const G3_1D_TWO_ROWS: [u8; 2] = [0xB7, 0x6C];

    #[test]
    fn group3_1d_black_is_1_false_maps_coded_black_to_zero() {
        let image = image(8, 2, &[("K", PdfValue::Integer(0))]);
        assert_eq!(
            decode(&G3_1D_TWO_ROWS, &image),
            [[W, W, W, W, B, B, B, B]; 2].concat()
        );
    }

    #[test]
    fn group3_1d_black_is_1_true_inverts_coded_colors() {
        let image = image(
            8,
            2,
            &[
                ("K", PdfValue::Integer(0)),
                ("BlackIs1", PdfValue::Bool(true)),
            ],
        );
        assert_eq!(
            decode(&G3_1D_TWO_ROWS, &image),
            [[B, B, B, B, W, W, W, W]; 2].concat()
        );
    }

    #[test]
    fn group3_2d_k_positive_decodes_tagged_1d_and_2d_rows() {
        // Row 1: tag 1, 1D `1011` `011` (4 white, 4 black).
        // Row 2: tag 0, 2D VL2 `000010` then V0 `1` (2 white, 6 black).
        let raw = [0xDB, 0x05];
        let image = image(8, 2, &[("K", PdfValue::Integer(4))]);
        assert_eq!(
            decode(&raw, &image),
            [[W, W, W, W, B, B, B, B], [W, W, B, B, B, B, B, B]].concat()
        );
    }

    #[test]
    fn group3_2d_black_is_1_true_inverts_coded_colors() {
        let raw = [0xDB, 0x05];
        let image = image(
            8,
            2,
            &[
                ("K", PdfValue::Integer(1)),
                ("BlackIs1", PdfValue::Bool(true)),
            ],
        );
        assert_eq!(
            decode(&raw, &image),
            [[B, B, B, B, W, W, W, W], [B, B, W, W, W, W, W, W]].concat()
        );
    }

    #[test]
    fn group4_row_ending_black_fills_to_the_last_column() {
        // Row 1: H `001`, white 2 `0111`, black 6 `0010`: changes [2].
        // Row 2: V0 `1`, V0 `1`: same as row 1. Then EOFB.
        let raw = [0x2E, 0x58, 0x00, 0x80, 0x08];
        let image = image(8, 2, &[("K", PdfValue::Integer(-1))]);
        assert_eq!(decode(&raw, &image), [[W, W, B, B, B, B, B, B]; 2].concat());
    }

    #[test]
    fn rows_decoded_before_an_error_are_kept_and_the_rest_padded() {
        // Row 1: 4 white, 4 black. Row 2: white 0 `00110101`, black 4 `011`,
        // then the data ends inside the row.
        //
        // hayro-ccitt 0.2 pushed pixels one at a time, so the 4 black pixels of
        // the unfinished row 2 reached the image. 0.5 pushes whole rows only,
        // so the unfinished row is padded like the rows after it.
        let raw = [0xB6, 0x6A, 0xC0];
        let image = image(8, 3, &[("K", PdfValue::Integer(0))]);
        assert_eq!(
            decode(&raw, &image),
            [[W, W, W, W, B, B, B, B], [W; 8], [W; 8]].concat()
        );
    }

    #[test]
    fn truncated_group4_drops_the_unfinished_row() {
        // Row 1: H `001`, white 2 `0111`, black 6 `0010`: changes [2].
        // Row 2: V0 `1` (2 white), VL1 `010` (5 black), then the data ends.
        //
        // hayro-ccitt 0.2 kept the 7 pixels of row 2 and padded the 8th white.
        // 0.5.0 returns the error before handing row 2 to the sink.
        let raw = [0x2E, 0x54];
        let image = image(8, 3, &[("K", PdfValue::Integer(-1))]);
        assert_eq!(
            decode(&raw, &image),
            [[W, W, B, B, B, B, B, B], [W; 8], [W; 8]].concat()
        );
    }

    #[test]
    fn group3_damaged_line_with_eols_is_recovered() {
        // EOL, row 1 (4 white, 4 black), EOL, row 2: white 0, black 4, then
        // the invalid code `000000001`, EOL, row 3 like row 1.
        //
        // hayro-ccitt 0.5 fills the rest of the damaged row white and resumes
        // at the next EOL. 0.2 stopped at the damaged row, so row 3 was padding.
        let raw = [0x00, 0x1B, 0x60, 0x02, 0x6A, 0xC0, 0x20, 0x03, 0x6C];
        let image = image(
            8,
            3,
            &[
                ("K", PdfValue::Integer(0)),
                ("EndOfLine", PdfValue::Bool(true)),
            ],
        );
        assert_eq!(
            decode(&raw, &image),
            [
                [W, W, W, W, B, B, B, B],
                [B, B, B, B, W, W, W, W],
                [W, W, W, W, B, B, B, B],
            ]
            .concat()
        );
    }

    #[test]
    fn padding_stays_white_when_black_is_1() {
        // `/Columns 4` inside an 8-pixel `/Width`, and 3 rows for 2 coded rows.
        // Row 1 and 2: 2 white `0111`, 2 black `11`.
        let raw = [0x7D, 0xF0];
        let image = image(
            8,
            3,
            &[
                ("K", PdfValue::Integer(0)),
                ("Columns", PdfValue::Integer(4)),
                ("Rows", PdfValue::Integer(2)),
                ("BlackIs1", PdfValue::Bool(true)),
            ],
        );
        assert_eq!(
            decode(&raw, &image),
            [[B, B, W, W, W, W, W, W], [B, B, W, W, W, W, W, W], [W; 8]].concat()
        );
    }

    #[test]
    fn columns_wider_than_width_are_clipped() {
        // `/Columns 8` coded into a 4-pixel `/Width`: 4 white, 4 black per row.
        let image = image(
            4,
            2,
            &[
                ("K", PdfValue::Integer(0)),
                ("Columns", PdfValue::Integer(8)),
            ],
        );
        assert_eq!(decode(&G3_1D_TWO_ROWS, &image), [W; 8]);
    }

    #[test]
    fn hostile_dimensions_are_refused_before_allocation() {
        let image = image(i64::from(u32::MAX), i64::from(u32::MAX), &[]);
        let error = decode_ccitt_to_luma(&G3_1D_TWO_ROWS, &image)
            .err()
            .expect("oversized image must be refused");
        assert!(error.to_string().contains("pixel budget"), "{error}");
    }

    #[test]
    fn error_before_the_first_row_fails() {
        let image = image(8, 2, &[("K", PdfValue::Integer(0))]);
        assert!(decode_ccitt_to_luma(&[0x00, 0x00], &image).is_err());
    }
}
