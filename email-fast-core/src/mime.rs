use crate::{Attachment, EmailBody, ParseOptions};
use anyhow::{anyhow, ensure, Result};
use base64::Engine;
use mailparse::{body::Body, DispositionType, MailHeaderMap, ParsedMail};
use std::io::{self, Read, Write};

pub(crate) type AttachmentSink<'a> =
    dyn FnMut(&Attachment, &mut dyn Read, usize) -> Result<usize> + 'a;

pub(crate) fn check_root_headers(bytes: &[u8], limit: usize) -> Result<()> {
    let mut length = 0;
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        length += line.len();
        ensure!(length <= limit, "MIME headers exceed max_header_bytes");
        if line == b"\n" || line == b"\r\n" {
            break;
        }
    }
    Ok(())
}

pub(crate) fn validate_tree(root: &ParsedMail<'_>, options: &ParseOptions) -> Result<()> {
    let mut stack = vec![(root, 1usize)];
    let mut count = 0;
    while let Some((part, depth)) = stack.pop() {
        count += 1;
        ensure!(
            count <= options.max_mime_parts,
            "MIME exceeds max_mime_parts"
        );
        ensure!(
            depth <= options.max_mime_depth,
            "MIME exceeds max_mime_depth"
        );
        check_root_headers(part.raw_bytes, options.max_header_bytes)?;
        stack.extend(part.subparts.iter().map(|child| (child, depth + 1)));
    }
    Ok(())
}

pub(crate) fn parse<'a, 'b>(
    root: &ParsedMail<'_>,
    options: ParseOptions,
    sink: Option<&'a mut AttachmentSink<'b>>,
) -> Result<(EmailBody, Vec<Attachment>)> {
    let mut context = Context {
        options,
        attachments: Vec::new(),
        total: 0,
        attachment_total: 0,
        sink,
    };
    let body = context.part(root, true, false)?;
    Ok((body, context.attachments))
}

struct Context<'a, 'b> {
    options: ParseOptions,
    attachments: Vec<Attachment>,
    total: usize,
    attachment_total: usize,
    sink: Option<&'a mut AttachmentSink<'b>>,
}

fn content_id(part: &ParsedMail<'_>) -> Option<String> {
    part.headers
        .get_first_value("Content-ID")
        .map(|s| s.trim().trim_matches(['<', '>']).to_owned())
        .filter(|s| !s.is_empty())
}

impl Context<'_, '_> {
    fn part(
        &mut self,
        part: &ParsedMail<'_>,
        body_allowed: bool,
        related_root: bool,
    ) -> Result<EmailBody> {
        let disposition = part.get_content_disposition();
        let mime = part.ctype.mimetype.to_ascii_lowercase();
        let filename = disposition
            .params
            .get("filename")
            .or_else(|| part.ctype.params.get("name"))
            .cloned();
        let explicit_attachment = disposition.disposition == DispositionType::Attachment;
        let multipart = !part.subparts.is_empty()
            || (mime.starts_with("multipart") && part.ctype.params.contains_key("boundary"));
        if multipart
            && body_allowed
            && (related_root || (!explicit_attachment && filename.is_none()))
        {
            let mut result = EmailBody::default();
            let root = if mime == "multipart/related" {
                if let Some(start) = part.ctype.params.get("start") {
                    Some(
                        part.subparts
                            .iter()
                            .position(|p| {
                                content_id(p).as_deref() == Some(start.trim_matches(['<', '>']))
                            })
                            .ok_or_else(|| {
                                anyhow!("multipart/related start does not identify a part")
                            })?,
                    )
                } else {
                    Some(0)
                }
            } else {
                None
            };
            for (index, child) in part.subparts.iter().enumerate() {
                let body = self.part(
                    child,
                    root.is_none_or(|root| root == index),
                    root == Some(index),
                )?;
                if mime != "multipart/alternative" {
                    self.total += usize::from(result.text.is_some() && body.text.is_some())
                        + usize::from(result.html.is_some() && body.html.is_some());
                    ensure!(
                        self.total <= self.options.max_inflate,
                        "decoded bodies exceed max_inflate"
                    );
                }
                merge_body(&mut result, body, mime == "multipart/alternative");
            }
            return Ok(result);
        }
        let id = content_id(part);
        let is_text = mime == "text/plain" || mime == "text/html";
        // Content-ID identifies a MIME entity; it does not make a body an attachment.
        if body_allowed && is_text && (related_root || (!explicit_attachment && filename.is_none()))
        {
            let mut bytes = Vec::new();
            copy_limited(
                &mut decoded_reader(part)?,
                &mut bytes,
                self.options.max_inflate.saturating_sub(self.total),
            )?;
            let text = if encoding_rs::Encoding::for_label(part.ctype.charset.as_bytes()).is_none()
            {
                crate::decode_unlabeled_bytes(&bytes)
            } else {
                let body = Body::new(&bytes, &part.ctype, &None);
                match body {
                    Body::SevenBit(body) | Body::EightBit(body) => body.get_as_string()?,
                    Body::Binary(body) => body.get_as_string()?,
                    _ => unreachable!(),
                }
            };
            self.total = self
                .total
                .checked_add(text.len())
                .ok_or_else(|| anyhow!("decoded size overflow"))?;
            ensure!(
                self.total <= self.options.max_inflate,
                "decoded bodies exceed max_inflate"
            );
            return Ok(if mime == "text/plain" {
                EmailBody {
                    text: Some(text),
                    html: None,
                }
            } else {
                EmailBody {
                    text: None,
                    html: Some(text),
                }
            });
        }
        let is_inline = !explicit_attachment
            && (id.is_some()
                || !body_allowed
                || (part
                    .headers
                    .get_first_header("Content-Disposition")
                    .is_some()
                    && disposition.disposition == DispositionType::Inline));
        let mut attachment = Attachment {
            filename: filename.unwrap_or_else(|| crate::fallback_filename(&id, &mime)),
            content_type: mime,
            content_id: id,
            is_inline,
            size: 0,
            content: None,
        };
        let limit = self
            .options
            .max_attachment_size
            .min(
                self.options
                    .max_total_attachment_bytes
                    .saturating_sub(self.attachment_total),
            )
            .min(self.options.max_inflate.saturating_sub(self.total));
        let mut reader: Box<dyn Read + '_> = if multipart {
            Box::new(part.raw_bytes)
        } else {
            decoded_reader(part)?
        };
        let size = if let Some(sink) = &mut self.sink {
            sink(&attachment, &mut reader, limit)?
        } else if self.options.extract_attachment_content {
            let mut bytes = Vec::new();
            let size = copy_limited(&mut reader, &mut bytes, limit)?;
            attachment.content = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            size
        } else {
            copy_limited(&mut reader, &mut io::sink(), limit)?
        };
        attachment.size = size;
        self.total += size;
        self.attachment_total += size;
        self.attachments.push(attachment);
        Ok(EmailBody::default())
    }
}

fn merge_body(target: &mut EmailBody, body: EmailBody, alternative: bool) {
    for (slot, value) in [(&mut target.text, body.text), (&mut target.html, body.html)] {
        if let Some(value) = value {
            if alternative || slot.is_none() {
                *slot = Some(value);
            } else if let Some(existing) = slot {
                existing.push('\n');
                existing.push_str(&value);
            }
        }
    }
}

pub(crate) fn copy_limited(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    limit: usize,
) -> Result<usize> {
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n)
            .ok_or_else(|| anyhow!("decoded size overflow"))?;
        ensure!(
            total <= limit,
            "decoded content exceeds attachment or max_inflate budget"
        );
        writer.write_all(&buffer[..n])?;
    }
    Ok(total)
}

struct NonWhitespace<'a>(&'a [u8]);
impl Read for NonWhitespace<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut count = 0;
        while count < out.len() && !self.0.is_empty() {
            let b = self.0[0];
            self.0 = &self.0[1..];
            if !b.is_ascii_whitespace() {
                out[count] = b;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn decoded_reader<'a>(part: &'a ParsedMail<'a>) -> Result<Box<dyn Read + 'a>> {
    use base64::engine::{
        general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig,
    };
    static ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    Ok(match part.get_body_encoded() {
        Body::Base64(body) => Box::new(base64::read::DecoderReader::new(
            NonWhitespace(body.get_raw()),
            &ENGINE,
        )),
        Body::QuotedPrintable(body) => Box::new(QuotedPrintable {
            bytes: body.get_raw(),
            whitespace: 0,
        }),
        Body::SevenBit(body) | Body::EightBit(body) => Box::new(body.get_raw()),
        Body::Binary(body) => Box::new(body.get_raw()),
    })
}

// Decode quoted-printable incrementally; soft breaks and trailing line whitespace
// follow RFC 2045. Malformed escape sequences remain literal, as in mailparse's
// robust mode. No allocation depends on the encoded attachment's length.
struct QuotedPrintable<'a> {
    bytes: &'a [u8],
    whitespace: usize,
}
impl Read for QuotedPrintable<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < output.len() && !self.bytes.is_empty() {
            if self.whitespace > 0 {
                let count = self.whitespace.min(output.len() - written);
                output[written..written + count].copy_from_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                self.whitespace -= count;
                written += count;
                continue;
            }
            let bytes = self.bytes;
            if bytes[0] == b'=' {
                if bytes.starts_with(b"=\r\n") {
                    self.bytes = &bytes[3..];
                    continue;
                }
                if bytes.starts_with(b"=\n") {
                    self.bytes = &bytes[2..];
                    continue;
                }
                if let (Some(a), Some(b)) = (
                    bytes.get(1).and_then(|b| (*b as char).to_digit(16)),
                    bytes.get(2).and_then(|b| (*b as char).to_digit(16)),
                ) {
                    output[written] = ((a << 4) | b) as u8;
                    written += 1;
                    self.bytes = &bytes[3..];
                    continue;
                }
            }
            if matches!(bytes[0], b' ' | b'\t') {
                let end = bytes
                    .iter()
                    .position(|b| !matches!(b, b' ' | b'\t'))
                    .unwrap_or(bytes.len());
                if end == bytes.len() || bytes[end..].starts_with(b"\r\n") || bytes[end] == b'\n' {
                    self.bytes = &bytes[end..];
                    continue;
                }
                self.whitespace = end;
                continue;
            }
            output[written] = bytes[0];
            written += 1;
            self.bytes = &bytes[1..];
        }
        Ok(written)
    }
}
