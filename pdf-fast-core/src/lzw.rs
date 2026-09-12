//! LZW decoder for the PDF `LZWDecode` filter.
//!
//! Implements variable-width LZW as described in PDF 32000-1:2008 § 7.4.4:
//! MSB-first packed codes, code length starts at 9 bits and grows up to 12,
//! `EarlyChange` parameter (default 1) bumps the code length one code earlier
//! than the strict TIFF behaviour, the clear code (256) resets the dictionary,
//! and the EOD code (257) terminates the stream.
//!
//! The optional PNG-style `Predictor` post-processing step is implemented in
//! [`apply_predictor`] and is applied by the caller when `DecodeParms /Predictor`
//! is greater than 1.

use anyhow::{anyhow, Result};

const CLEAR_CODE: u32 = 256;
const EOD_CODE: u32 = 257;
const FIRST_DICT_CODE: u32 = 258;
const MAX_CODE_LEN: u8 = 12;
const MAX_TABLE_SIZE: u32 = 4096;

/// Decode an LZW-compressed payload.
///
/// `early_change` is the value of the `EarlyChange` decode parameter — `1`
/// (the PDF default) means the code length grows one code earlier than the
/// strict TIFF behaviour.  Pass `0` to match TIFF.
pub fn decode_lzw(raw: &[u8], early_change: u8) -> Result<Vec<u8>> {
    decode_lzw_limited(raw, early_change, usize::MAX)
}

/// Decode an LZW payload while refusing output larger than `max_len`.
///
/// The limit is checked before extending the output buffer, so hostile or
/// corrupt streams cannot force an unbounded allocation before the caller's
/// PDF stream limit is enforced.
pub fn decode_lzw_limited(raw: &[u8], early_change: u8, max_len: usize) -> Result<Vec<u8>> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let early = if early_change == 0 { 0u32 } else { 1u32 };
    let mut bits = BitReader::new(raw);
    let mut code_len: u8 = 9;
    let mut next_code: u32 = FIRST_DICT_CODE;
    let mut dict: Vec<Vec<u8>> = Vec::with_capacity(MAX_TABLE_SIZE as usize);
    init_dict(&mut dict);

    let mut out: Vec<u8> = Vec::with_capacity(raw.len() * 4);
    let mut prev: Option<Vec<u8>> = None;

    while let Some(code) = bits.read_bits(code_len) {
        if code == EOD_CODE {
            break;
        }
        if code == CLEAR_CODE {
            code_len = 9;
            next_code = FIRST_DICT_CODE;
            init_dict(&mut dict);
            prev = None;
            continue;
        }

        let entry: Vec<u8> = if let Some(s) = dict.get(code as usize) {
            if s.is_empty() && code >= FIRST_DICT_CODE {
                // Reserved / not-yet-assigned slot
                if code == next_code {
                    let p = prev
                        .as_ref()
                        .ok_or_else(|| anyhow!("LZW: KwKwK without previous code"))?;
                    let mut e = p.clone();
                    e.push(p[0]);
                    e
                } else {
                    return Err(anyhow!(
                        "LZW: code {} references empty dict slot (next={}, code_len={})",
                        code,
                        next_code,
                        code_len
                    ));
                }
            } else {
                s.clone()
            }
        } else if code == next_code {
            // KwKwK case: code refers to the entry we are about to add.
            let p = prev
                .as_ref()
                .ok_or_else(|| anyhow!("LZW: KwKwK without previous code"))?;
            let mut e = p.clone();
            e.push(p[0]);
            e
        } else {
            return Err(anyhow!(
                "LZW: invalid code {} (next={}, dict_len={}, code_len={})",
                code,
                next_code,
                dict.len(),
                code_len,
            ));
        };

        let next_len = out
            .len()
            .checked_add(entry.len())
            .ok_or_else(|| anyhow!("LZW decoded output size overflow"))?;
        if next_len > max_len {
            return Err(anyhow!(
                "LZW decoded stream len exceeds max_decoded_stream_len {}",
                max_len
            ));
        }
        out.extend_from_slice(&entry);

        if let Some(p) = &prev {
            if next_code < MAX_TABLE_SIZE {
                let mut new_entry = Vec::with_capacity(p.len() + 1);
                new_entry.extend_from_slice(p);
                new_entry.push(entry[0]);
                if (next_code as usize) >= dict.len() {
                    dict.push(new_entry);
                } else {
                    dict[next_code as usize] = new_entry;
                }
                next_code += 1;
                if code_len < MAX_CODE_LEN && next_code + early >= (1u32 << code_len) {
                    code_len += 1;
                }
            }
        }

        prev = Some(entry);
    }

    Ok(out)
}

fn init_dict(dict: &mut Vec<Vec<u8>>) {
    dict.clear();
    for byte in 0u32..256 {
        dict.push(vec![byte as u8]);
    }
    dict.push(Vec::new()); // 256 — clear (placeholder)
    dict.push(Vec::new()); // 257 — EOD (placeholder)
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let total_bits = self.data.len().saturating_mul(8);
        if self.bit_pos + n as usize > total_bits {
            return None;
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            let bit_idx = 7 - (self.bit_pos & 7);
            let bit = (self.data[byte_idx] >> bit_idx) & 1;
            value = (value << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Some(value)
    }
}

/// Apply the PNG-style row predictor described in PDF 32000-1 § 7.4.4.4.
///
/// `predictor` values:
/// - `1` → no prediction (returned unchanged)
/// - `2` → TIFF predictor (left)
/// - `10..=15` → PNG predictors (None / Sub / Up / Average / Paeth / Optimum)
///
/// `columns` is the number of pixels per row, `colors` is the number of
/// interleaved color components, and `bits_per_component` is the component
/// bit depth. PNG prediction works on bytes for all PDF bit depths; TIFF
/// prediction additionally handles packed 1-, 2-, and 4-bit samples and
/// big-endian 16-bit samples.
pub fn apply_predictor(
    data: &[u8],
    predictor: u8,
    columns: usize,
    colors: usize,
    bits_per_component: usize,
) -> Result<Vec<u8>> {
    if predictor <= 1 {
        return Ok(data.to_vec());
    }
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if (10..=15).contains(&predictor) && colors >= 1 && columns >= 1 {
        if !matches!(bits_per_component, 1 | 2 | 4 | 8 | 16) {
            return Err(anyhow!(
                "unsupported PNG predictor BitsPerComponent {}",
                bits_per_component
            ));
        }
        let bits_per_pixel = colors
            .checked_mul(bits_per_component)
            .ok_or_else(|| anyhow!("predictor bits-per-pixel overflow"))?;
        let bpp = bits_per_pixel.div_ceil(8).max(1);
        let row_bits = columns
            .checked_mul(bits_per_pixel)
            .ok_or_else(|| anyhow!("predictor row size overflow"))?;
        let row_len = row_bits.div_ceil(8);
        let stride = row_len
            .checked_add(1)
            .ok_or_else(|| anyhow!("predictor stride overflow"))?; // 1-byte tag per row
        if !data.len().is_multiple_of(stride) {
            return Err(anyhow!(
                "predictor row length mismatch: data={} stride={}",
                data.len(),
                stride
            ));
        }
        let mut out = Vec::with_capacity(data.len() - data.len() / stride);
        let row_count = data.len() / stride;
        let mut prev_row = vec![0u8; row_len];
        for r in 0..row_count {
            let row = &data[r * stride..r * stride + stride];
            let tag = row[0];
            let body = &row[1..];
            let mut current = vec![0u8; row_len];
            match tag {
                0 => current.copy_from_slice(body),
                1 => {
                    for i in 0..row_len {
                        let left = if i >= bpp { current[i - bpp] } else { 0 };
                        current[i] = body[i].wrapping_add(left);
                    }
                }
                2 => {
                    for i in 0..row_len {
                        current[i] = body[i].wrapping_add(prev_row[i]);
                    }
                }
                3 => {
                    for i in 0..row_len {
                        let left = if i >= bpp { current[i - bpp] as u16 } else { 0 };
                        let up = prev_row[i] as u16;
                        current[i] = body[i].wrapping_add(((left + up) / 2) as u8);
                    }
                }
                4 => {
                    for i in 0..row_len {
                        let left = if i >= bpp { current[i - bpp] as i16 } else { 0 };
                        let up = prev_row[i] as i16;
                        let upper_left = if i >= bpp {
                            prev_row[i - bpp] as i16
                        } else {
                            0
                        };
                        let p = left + up - upper_left;
                        let pa = (p - left).abs();
                        let pb = (p - up).abs();
                        let pc = (p - upper_left).abs();
                        let predicted = if pa <= pb && pa <= pc {
                            left
                        } else if pb <= pc {
                            up
                        } else {
                            upper_left
                        };
                        current[i] = body[i].wrapping_add(predicted as u8);
                    }
                }
                _ => {
                    return Err(anyhow!("unsupported PNG predictor tag {}", tag));
                }
            }
            out.extend_from_slice(&current);
            prev_row = current;
        }
        return Ok(out);
    }
    if predictor == 2 && colors >= 1 && columns >= 1 {
        if !matches!(bits_per_component, 1 | 2 | 4 | 8 | 16) {
            return Err(anyhow!(
                "unsupported TIFF predictor BitsPerComponent {}",
                bits_per_component
            ));
        }
        let samples_per_row = columns
            .checked_mul(colors)
            .ok_or_else(|| anyhow!("TIFF predictor sample count overflow"))?;
        let row_len = samples_per_row
            .checked_mul(bits_per_component)
            .ok_or_else(|| anyhow!("TIFF predictor row size overflow"))?
            .div_ceil(8);
        if !data.len().is_multiple_of(row_len) {
            return Err(anyhow!("TIFF predictor row length mismatch"));
        }
        let mut out = Vec::with_capacity(data.len());
        for row in data.chunks(row_len) {
            if bits_per_component == 8 {
                let mut current = row.to_vec();
                for i in colors..row_len {
                    current[i] = current[i].wrapping_add(current[i - colors]);
                }
                out.extend_from_slice(&current);
            } else {
                let mut samples = unpack_tiff_samples(row, samples_per_row, bits_per_component);
                let modulus = 1u32 << bits_per_component;
                for i in colors..samples.len() {
                    samples[i] = (samples[i] + samples[i - colors]) % modulus;
                }
                pack_tiff_samples(&samples, bits_per_component, &mut out);
            }
        }
        return Ok(out);
    }
    Err(anyhow!("unsupported Predictor value {}", predictor))
}

fn unpack_tiff_samples(row: &[u8], sample_count: usize, bits_per_component: usize) -> Vec<u32> {
    let mut samples = Vec::with_capacity(sample_count);
    let mask = (1u32 << bits_per_component) - 1;
    for sample_index in 0..sample_count {
        let bit_offset = sample_index * bits_per_component;
        let mut value = 0u32;
        for bit in 0..bits_per_component {
            let absolute = bit_offset + bit;
            let source = (row[absolute / 8] >> (7 - absolute % 8)) & 1;
            value = (value << 1) | u32::from(source);
        }
        samples.push(value & mask);
    }
    samples
}

fn pack_tiff_samples(samples: &[u32], bits_per_component: usize, out: &mut Vec<u8>) {
    let row_start = out.len();
    let row_len = (samples.len() * bits_per_component).div_ceil(8);
    out.resize(row_start + row_len, 0);
    for (sample_index, sample) in samples.iter().enumerate() {
        let bit_offset = sample_index * bits_per_component;
        for bit in 0..bits_per_component {
            let source_shift = bits_per_component - bit - 1;
            let value = ((sample >> source_shift) & 1) as u8;
            let absolute = bit_offset + bit;
            out[row_start + absolute / 8] |= value << (7 - absolute % 8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Reference encoder used only by the decoder unit tests.  Produces a
    /// PDF-compatible LZW bitstream that matches the asymmetric bump timing
    /// observed in real-world encoders (libtiff, Acrobat Distiller): the
    /// encoder advances the code length **one insert later** than the spec's
    /// "EarlyChange=1" interpretation suggests, because the encoder's
    /// write-then-insert order skips one round-trip of dictionary growth
    /// compared to the decoder's read-then-insert order.  Concretely, the
    /// encoder bumps when `next_code >= 1 << code_len` (no early offset)
    /// while the decoder bumps when `next_code + early >= 1 << code_len`
    /// (early=1).  Both end up applying the new width to the same code
    /// position in the bit stream.
    fn encode_lzw_for_tests(input: &[u8]) -> Vec<u8> {
        let mut bw = BitWriter::new();
        let mut code_len: u8 = 9;
        let mut next_code: u32 = FIRST_DICT_CODE;
        let mut dict: HashMap<Vec<u8>, u32> = HashMap::new();
        for i in 0u32..256 {
            dict.insert(vec![i as u8], i);
        }
        bw.write_bits(CLEAR_CODE, code_len);
        let mut w: Vec<u8> = Vec::new();
        for &byte in input {
            let mut wb = w.clone();
            wb.push(byte);
            if dict.contains_key(&wb) {
                w = wb;
            } else {
                let prev_code = *dict.get(&w).unwrap();
                bw.write_bits(prev_code, code_len);
                if next_code < MAX_TABLE_SIZE {
                    dict.insert(wb, next_code);
                    next_code += 1;
                    if code_len < MAX_CODE_LEN && next_code >= (1u32 << code_len) {
                        code_len += 1;
                    }
                }
                w = vec![byte];
            }
        }
        if !w.is_empty() {
            let last_code = *dict.get(&w).unwrap();
            bw.write_bits(last_code, code_len);
        }
        bw.write_bits(EOD_CODE, code_len);
        bw.finish()
    }

    struct BitWriter {
        bytes: Vec<u8>,
        bit_pos: u8,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit_pos: 0,
            }
        }

        fn write_bits(&mut self, mut value: u32, mut n: u8) {
            value &= (1u64 << n) as u32 - 1;
            while n > 0 {
                if self.bit_pos == 0 {
                    self.bytes.push(0);
                }
                let avail = 8 - self.bit_pos;
                let take = avail.min(n);
                let shift = n - take;
                let frag = ((value >> shift) & ((1u32 << take) - 1)) as u8;
                let last = self.bytes.last_mut().unwrap();
                *last |= frag << (avail - take);
                self.bit_pos = (self.bit_pos + take) & 7;
                n -= take;
                if shift == 0 {
                    value = 0;
                } else {
                    value &= (1u32 << shift) - 1;
                }
            }
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    #[test]
    fn round_trips_short_string() {
        let payload = b"Hi";
        let encoded = encode_lzw_for_tests(payload);
        let decoded = decode_lzw(&encoded, 1).expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn round_trips_repeated_string() {
        let payload = b"HelloHelloHello";
        let encoded = encode_lzw_for_tests(payload);
        let decoded = decode_lzw(&encoded, 1).expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn round_trips_payload_just_past_first_width_growth() {
        // Build a payload that forces the dictionary just past code 510 so the
        // encoder must transition from 9-bit to 10-bit codes mid-stream.  Each
        // unique two-byte phrase grows the dictionary by exactly one entry, so
        // 260 distinct two-byte phrases push us cleanly over the threshold.
        let mut payload = Vec::new();
        for i in 0..260u32 {
            payload.push((i & 0xFF) as u8);
            payload.push(((i >> 8) & 0xFF) as u8);
        }
        let encoded = encode_lzw_for_tests(&payload);
        let (decoded, codes_read) = decode_with_trace(&encoded, 1);
        match decoded {
            Ok(d) => assert_eq!(d, payload, "round trip mismatch"),
            Err(e) => panic!("decode failed: {} after {} codes", e, codes_read),
        }
    }

    fn decode_with_trace(raw: &[u8], early_change: u8) -> (Result<Vec<u8>>, usize) {
        let early = if early_change == 0 { 0u32 } else { 1u32 };
        let mut bits = BitReader::new(raw);
        let mut code_len: u8 = 9;
        let mut next_code: u32 = FIRST_DICT_CODE;
        let mut dict: Vec<Vec<u8>> = Vec::with_capacity(MAX_TABLE_SIZE as usize);
        super::init_dict(&mut dict);
        let mut out: Vec<u8> = Vec::new();
        let mut prev: Option<Vec<u8>> = None;
        let mut codes = 0usize;
        while let Some(code) = bits.read_bits(code_len) {
            codes += 1;
            if code == EOD_CODE {
                break;
            }
            if code == CLEAR_CODE {
                code_len = 9;
                next_code = FIRST_DICT_CODE;
                super::init_dict(&mut dict);
                prev = None;
                continue;
            }
            let entry: Vec<u8> = if let Some(s) = dict.get(code as usize) {
                if s.is_empty() && code >= FIRST_DICT_CODE {
                    if code == next_code {
                        let p = match prev.as_ref() {
                            Some(p) => p,
                            None => return (Err(anyhow!("KwKwK without prev")), codes),
                        };
                        let mut e = p.clone();
                        e.push(p[0]);
                        e
                    } else {
                        return (
                            Err(anyhow!(
                                "code {} empty (next={}, len={}, code_len={}, codes={})",
                                code,
                                next_code,
                                dict.len(),
                                code_len,
                                codes,
                            )),
                            codes,
                        );
                    }
                } else {
                    s.clone()
                }
            } else if code == next_code {
                let p = prev.as_ref().unwrap();
                let mut e = p.clone();
                e.push(p[0]);
                e
            } else {
                return (
                    Err(anyhow!(
                        "code {} oob (next={}, len={}, code_len={}, codes={})",
                        code,
                        next_code,
                        dict.len(),
                        code_len,
                        codes,
                    )),
                    codes,
                );
            };
            out.extend_from_slice(&entry);
            if let Some(p) = &prev {
                if next_code < MAX_TABLE_SIZE {
                    let mut new_entry = Vec::with_capacity(p.len() + 1);
                    new_entry.extend_from_slice(p);
                    new_entry.push(entry[0]);
                    if (next_code as usize) >= dict.len() {
                        dict.push(new_entry);
                    } else {
                        dict[next_code as usize] = new_entry;
                    }
                    next_code += 1;
                    if code_len < MAX_CODE_LEN && next_code + early >= (1u32 << code_len) {
                        code_len += 1;
                    }
                }
            }
            prev = Some(entry);
        }
        (Ok(out), codes)
    }

    #[test]
    fn round_trips_long_textual_payload() {
        let payload: Vec<u8> = (0..2000)
            .flat_map(|i| {
                let s = format!("Line {} - quick brown fox\n", i);
                s.into_bytes()
            })
            .collect();
        let encoded = encode_lzw_for_tests(&payload);
        let decoded = match decode_lzw(&encoded, 1) {
            Ok(d) => d,
            Err(e) => panic!("decode failed: {}", e),
        };
        assert_eq!(decoded.len(), payload.len());
        assert_eq!(decoded, payload);
    }

    #[test]
    fn round_trips_binary_payload() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let mut repeated = Vec::new();
        for _ in 0..40 {
            repeated.extend_from_slice(&payload);
        }
        let encoded = encode_lzw_for_tests(&repeated);
        let decoded = decode_lzw(&encoded, 1).expect("decode");
        assert_eq!(decoded, repeated);
    }

    #[test]
    fn empty_input_returns_empty() {
        let out = decode_lzw(&[], 1).expect("decode");
        assert!(out.is_empty());
    }

    #[test]
    fn lzw_limit_is_enforced_before_output_growth() {
        let payload = vec![b'A'; 4096];
        let encoded = encode_lzw_for_tests(&payload);
        let error = decode_lzw_limited(&encoded, 1, 64).unwrap_err();
        assert!(error.to_string().contains("max_decoded_stream_len 64"));
    }

    #[test]
    fn predictor_one_passes_through() {
        let data = b"hello world";
        let out = apply_predictor(data, 1, 11, 1, 8).expect("predictor");
        assert_eq!(out, data);
    }

    #[test]
    fn png_up_predictor_decodes_two_rows() {
        // Two rows of 4 bytes each, PNG-encoded.
        // Row 0: tag=0 (None) + raw [10, 20, 30, 40]
        // Row 1: tag=2 (Up)   + diffs [1, 1, 1, 1] → decoded [11, 21, 31, 41]
        let data = vec![0u8, 10, 20, 30, 40, 2, 1, 1, 1, 1];
        let out = apply_predictor(&data, 10, 4, 1, 8).expect("predictor");
        assert_eq!(out, vec![10, 20, 30, 40, 11, 21, 31, 41]);
    }

    #[test]
    fn tiff_predictor_decodes_packed_one_bit_samples() {
        let out = apply_predictor(&[0xff], 2, 8, 1, 1).expect("predictor");
        assert_eq!(out, vec![0xaa]);
    }

    #[test]
    fn tiff_predictor_decodes_big_endian_sixteen_bit_samples() {
        let out = apply_predictor(&[0x03, 0xe8, 0x01, 0xf4], 2, 2, 1, 16).expect("predictor");
        assert_eq!(out, vec![0x03, 0xe8, 0x05, 0xdc]);
    }
}
