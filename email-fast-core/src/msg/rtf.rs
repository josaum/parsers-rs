use anyhow::{anyhow, ensure, Result};

// MS-OXRTFCP: bounded LZFu dictionary decoding. Unlike a declared-size-only
// allocation guard, every emitted literal/reference is checked against RAWSIZE.
fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    ensure!(data.len() >= 16, "truncated compressed RTF header");
    let field = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    ensure!(
        field(0) as usize == data.len() - 4,
        "RTF compressed size mismatch"
    );
    let raw = field(4) as usize;
    ensure!(raw <= limit, "RTF exceeds max_inflate");
    if field(8) == 0x414C454D {
        ensure!(raw == data.len() - 16, "uncompressed RTF size mismatch");
        return Ok(data[16..].to_vec());
    }
    ensure!(field(8) == 0x75465A4C, "unsupported RTF compression");
    let mut crc = 0u32;
    for &byte in &data[16..] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xEDB88320 } else { 0 };
        }
    }
    ensure!(crc == field(12), "compressed RTF CRC mismatch");
    let initial=b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";
    let mut dictionary = [0u8; 4096];
    dictionary[..initial.len()].copy_from_slice(initial);
    let mut write = initial.len();
    let mut pos = 16;
    let mut out = Vec::new();
    let mut ended = false;
    'runs: while pos < data.len() {
        let control = data[pos];
        pos += 1;
        for bit in 0..8 {
            if control & (1 << bit) == 0 {
                let &byte = data
                    .get(pos)
                    .ok_or_else(|| anyhow!("truncated RTF literal"))?;
                pos += 1;
                ensure!(out.len() < raw, "RTF expands beyond declared size");
                out.push(byte);
                dictionary[write] = byte;
                write = (write + 1) % 4096;
            } else {
                let pair = data
                    .get(pos..pos + 2)
                    .ok_or_else(|| anyhow!("truncated RTF reference"))?;
                pos += 2;
                let reference = u16::from_be_bytes([pair[0], pair[1]]);
                let mut offset = (reference >> 4) as usize;
                let length = (reference & 15) as usize + 2;
                if offset == write {
                    ended = true;
                    break 'runs;
                }
                ensure!(
                    out.len().saturating_add(length) <= raw,
                    "RTF expands beyond declared size"
                );
                for _ in 0..length {
                    let byte = dictionary[offset];
                    offset = (offset + 1) % 4096;
                    out.push(byte);
                    dictionary[write] = byte;
                    write = (write + 1) % 4096;
                }
            }
        }
    }
    ensure!(ended && out.len() == raw, "incomplete compressed RTF");
    Ok(out)
}

pub(super) fn text(data: &[u8], limit: usize, max_depth: usize) -> Result<String> {
    let bytes = decompress(data, limit)?;
    let rtf = normalize_unicode(&bytes, limit, max_depth)?;
    let document = std::panic::catch_unwind(|| rtf_parser::RtfDocument::try_from(rtf.as_str()))
        .map_err(|_| anyhow!("malformed RTF"))?
        .map_err(|e| anyhow!("invalid RTF: {e:?}"))?;
    let text = document.get_text();
    ensure!(text.len() <= limit, "RTF text exceeds max_inflate");
    Ok(text)
}
use anyhow::Context;

// Normalize Unicode fallback runs before rtf-parser sees them. Its Unicode path
// otherwise retains literal fallbacks and allocates from an unchecked uc value.
// Setting uc=0 after removing fallbacks also preserves surrogate pairs.
fn normalize_unicode(bytes: &[u8], limit: usize, max_depth: usize) -> Result<String> {
    let mut output = Vec::new();
    let mut stack = Vec::new();
    let mut uc = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                stack.push(uc);
                ensure!(stack.len() <= max_depth, "RTF exceeds max_mime_depth");
                output.push(b'{');
                i += 1;
            }
            b'}' => {
                uc = stack.pop().ok_or_else(|| anyhow!("unbalanced RTF group"))?;
                output.push(b'}');
                i += 1;
            }
            b'\\' if bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic) => {
                let start = i;
                i += 1;
                let word_start = i;
                while bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                let word = &bytes[word_start..i];
                let number_start = i;
                if bytes.get(i) == Some(&b'-') {
                    i += 1;
                }
                while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                let number = if i > number_start {
                    Some(std::str::from_utf8(&bytes[number_start..i])?.parse::<i32>()?)
                } else {
                    None
                };
                let token_end = i;
                if bytes.get(i) == Some(&b' ') {
                    i += 1;
                }
                match word {
                    b"rtf" => {
                        output.extend_from_slice(&bytes[start..i]);
                        output.extend_from_slice(b"\\uc0 ");
                    }
                    b"uc" => {
                        let n = number.ok_or_else(|| anyhow!("missing RTF uc value"))?;
                        ensure!((0..=32767).contains(&n), "invalid RTF uc value");
                        uc = n as usize;
                        output.extend_from_slice(b"\\uc0 ");
                    }
                    b"u" => {
                        ensure!(
                            number.is_some_and(|n| (-32768..=65535).contains(&n)),
                            "invalid RTF Unicode value"
                        );
                        output.extend_from_slice(&bytes[start..token_end]);
                        output.push(b' ');
                        let mut skipped = 0;
                        while skipped < uc && i < bytes.len() && !matches!(bytes[i], b'{' | b'}') {
                            if matches!(bytes[i], b'\r' | b'\n') {
                                i += 1;
                                continue;
                            }
                            if bytes[i] == b'\\' {
                                i += 1;
                                ensure!(i < bytes.len(), "truncated RTF fallback");
                                if bytes[i] == b'\'' {
                                    ensure!(
                                        bytes
                                            .get(i + 1..i + 3)
                                            .is_some_and(|s| s.iter().all(u8::is_ascii_hexdigit)),
                                        "invalid RTF hex fallback"
                                    );
                                    i += 3;
                                } else if bytes[i].is_ascii_alphabetic() {
                                    while bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
                                        i += 1;
                                    }
                                    if bytes.get(i) == Some(&b'-') {
                                        i += 1;
                                    }
                                    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                                        i += 1;
                                    }
                                    if bytes.get(i) == Some(&b' ') {
                                        i += 1;
                                    }
                                } else {
                                    i += 1;
                                }
                            } else {
                                i += 1;
                            }
                            skipped += 1;
                        }
                    }
                    b"bin" => {
                        let n = number
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| anyhow!("invalid RTF binary length"))?;
                        ensure!(n <= bytes.len() - i, "truncated RTF binary data");
                        i += n;
                    }
                    _ => output.extend_from_slice(&bytes[start..i]),
                }
            }
            b'\\' => {
                ensure!(i + 1 < bytes.len(), "truncated RTF escape");
                output.extend_from_slice(&bytes[i..i + 2]);
                i += 2;
            }
            byte => {
                output.push(byte);
                i += 1;
            }
        }
        ensure!(output.len() <= limit, "normalized RTF exceeds max_inflate");
    }
    ensure!(stack.is_empty(), "unclosed RTF group");
    String::from_utf8(output).context("RTF must escape non-UTF-8 text")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rtf_preserves_hex_and_unicode_characters() {
        for rtf in [
            r"{\rtf1\ansi\ansicpg1252 caf\'e9}",
            r"{\rtf1\ansi\uc1 caf\u233?}",
        ] {
            let data = compressed_rtf::compress_rtf(rtf).unwrap();
            assert_eq!(text(&data, 4096, 64).unwrap().trim(), "café");
        }
    }

    #[test]
    fn unicode_fallback_state_and_surrogate_pairs() {
        for (rtf, expected) in [
            (r"{\rtf1\ansi \u233?\u233?}", "éé"),
            (r"{\rtf1\ansi \u-10179?\u-8704?}", "😀"),
            (r"{\rtf1\ansi\uc2 caf\u233\'65\'3f}", "café"),
        ] {
            let data = compressed_rtf::compress_rtf(rtf).unwrap();
            assert_eq!(text(&data, 4096, 64).unwrap().trim(), expected);
        }
        let data = compressed_rtf::compress_rtf(r"{\rtf1\uc-1 \u233?}").unwrap();
        assert!(text(&data, 4096, 64).is_err());
    }

    #[test]
    fn compressed_rtf_round_trip_and_budgets() {
        let rtf = r"{\rtf1\ansi Hello \b world\b0.}";
        let compressed = compressed_rtf::compress_rtf(rtf).unwrap();
        assert_eq!(decompress(&compressed, 4096).unwrap(), rtf.as_bytes());
        assert!(text(&compressed, 4096, 64).unwrap().contains("Hello world"));
        assert!(decompress(&compressed, 1).is_err());
        assert!(decompress(&compressed[..8], 4096).is_err());
        let mut bad = compressed;
        bad[12] ^= 1;
        assert!(decompress(&bad, 4096).is_err());
    }
}
