use crate::PdfImageInfo;
use anyhow::{anyhow, Result};
use hayro_ccitt::{DecodeSettings, Decoder, EncodingMode};
use image::ImageEncoder;

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
        encoding: if params.k < 0 {
            EncodingMode::Group4
        } else if params.k == 0 {
            EncodingMode::Group3_1D
        } else {
            EncodingMode::Group3_2D { k: params.k as u32 }
        },
        invert_black: params.black_is_1,
    };

    let mut sink = GraySink::new(width, height)?;
    let result = hayro_ccitt::decode(raw, &mut sink, &settings);
    if let Err(error) = result {
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

struct GraySink {
    width: u32,
    height: u32,
    decoded_rows: u32,
    col: u32,
    pixels: Vec<u8>,
}

impl GraySink {
    fn new(width: u32, height: u32) -> Result<Self> {
        const MAX_PIXELS: u64 = 100_000_000;
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| anyhow!("CCITT image dimensions overflow"))?;
        if pixels > MAX_PIXELS {
            return Err(anyhow!("CCITT image exceeds pixel budget"));
        }
        let len =
            usize::try_from(pixels).map_err(|_| anyhow!("CCITT image dimensions overflow"))?;
        Ok(Self {
            width,
            height,
            decoded_rows: 0,
            col: 0,
            pixels: Vec::with_capacity(len),
        })
    }

    fn push_run(&mut self, white: bool, count: u32) {
        if self.decoded_rows >= self.height {
            return;
        }
        let remaining = self.width.saturating_sub(self.col);
        let count = count.min(remaining);
        let value = if white { 0xFF } else { 0x00 };
        self.pixels
            .extend(std::iter::repeat_n(value, count as usize));
        self.col += count;
    }

    fn finish_line(&mut self) {
        if self.decoded_rows >= self.height {
            return;
        }
        if self.col < self.width {
            self.push_run(true, self.width - self.col);
        }
        self.decoded_rows += 1;
        self.col = 0;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.col > 0 {
            self.finish_line();
        }
        while self.decoded_rows < self.height {
            self.finish_line();
        }
        let expected = (self.width as usize).saturating_mul(self.height as usize);
        self.pixels.truncate(expected);
        self.pixels
    }
}

impl Decoder for GraySink {
    fn push_pixel(&mut self, white: bool) {
        self.push_run(white, 1);
    }

    fn push_pixel_chunk(&mut self, white: bool, chunk_count: u32) {
        self.push_run(white, chunk_count.saturating_mul(8));
    }

    fn next_line(&mut self) {
        self.finish_line();
    }
}
