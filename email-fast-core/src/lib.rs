#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # email-fast-core
//!
//! High-performance email parser for .eml and .msg formats.
//!
//! ## Features
//!
//! - **Fast**: Memory-mapped file access for large mailboxes
//! - **Complete**: Headers, body (text/html), attachments, threading
//! - **Multi-format**: EML (RFC 5322) and MSG (Outlook) support
//! - **Streaming**: Mbox format support for bulk processing
//! - **Safe**: Rust memory safety guarantees
//! - **Arrow IPC**: Export to Apache Arrow format (optional feature)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use email_fast_core::{parse_eml, parse_msg, ParseOptions};
//!
//! // Parse EML file
//! let email = parse_eml("message.eml", ParseOptions::default())?;
//! println!("From: {:?}", email.from);
//! println!("Subject: {:?}", email.subject);
//!
//! // Parse MSG file (Outlook)
//! let email = parse_msg("message.msg", ParseOptions::default())?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Input: .eml or .msg file                                   │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Memory-mapped file access (memmap2)                        │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Format detection → EML parser or MSG parser                │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Output: Email struct | NDJSON | Arrow IPC                  │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use memmap2::Mmap;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

#[cfg(feature = "arrow")]
pub mod arrow;

// The GLiNER extractor and the mycelia-api embedder that were declared here are
// consumer integrations: they depend on one product's SDK and belong in that
// product, not in a parser shared across consumers.

mod msg;

/// Hidden CFB fixture writer for integration tests (mini-stream MSG layout).
#[doc(hidden)]
pub fn write_msg_cfb_fixture(subject: &str, sender: &str, to: &str, body: &str) -> Vec<u8> {
    msg::write_fixture_msg(subject, sender, to, body)
}

/// Configuration options for email parsing.
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Max uncompressed part size to load into memory (bytes).
    ///
    /// Parts larger than this threshold are streamed rather than
    /// loaded entirely into memory. Default: 128 MiB.
    /// Mirrors the `max_inflate` field in the OOXML parsers for API
    /// consistency across all `*-fast-core` crates.
    pub max_inflate: usize,
    /// Maximum size of attachments to process in bytes (default: 100 MB)
    pub max_attachment_size: usize,
    /// Maximum decoded bytes retained across all attachments (default: 100 MB).
    ///
    /// The per-attachment ceiling above bounds one part; this bounds their
    /// sum, so a message with many individually in-budget parts cannot
    /// exhaust memory.
    pub max_total_attachment_bytes: usize,
    /// Include raw headers in output (default: false)
    pub include_raw_headers: bool,
    /// Parse HTML body to plain text (default: false)
    pub html_to_text: bool,
    /// Extract attachment content (not just metadata) (default: false)
    pub extract_attachment_content: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_inflate: 128 * 1024 * 1024,         // 128 MiB
            max_attachment_size: 100 * 1024 * 1024, // 100 MB
            max_total_attachment_bytes: 100 * 1024 * 1024, // 100 MB
            include_raw_headers: false,
            html_to_text: false,
            extract_attachment_content: false,
        }
    }
}

/// Email address with optional display name
#[derive(Debug, Clone, Serialize, Default)]
pub struct EmailAddress {
    /// Display name (e.g., "John Doe")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Email address (e.g., "john@example.com")
    pub address: String,
}

/// Email attachment
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    /// Filename
    pub filename: String,
    /// MIME type
    pub content_type: String,
    /// File size in bytes
    pub size: usize,
    /// Content ID for inline attachments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// Whether this is an inline attachment
    pub is_inline: bool,
    /// Base64-encoded content (only if extract_attachment_content is true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Email body content
#[derive(Debug, Clone, Serialize, Default)]
pub struct EmailBody {
    /// Plain text content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// HTML content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

/// Threading information for email conversations
#[derive(Debug, Clone, Serialize, Default)]
pub struct ThreadInfo {
    /// Message ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// In-Reply-To header
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// References header (comma-separated message IDs)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
}

/// Complete email message
#[derive(Debug, Clone, Serialize)]
pub struct Email {
    /// Message ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Sender address
    pub from: EmailAddress,
    /// Reply-To address
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<EmailAddress>,
    /// To recipients
    pub to: Vec<EmailAddress>,
    /// CC recipients
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<EmailAddress>,
    /// BCC recipients
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<EmailAddress>,
    /// Subject line
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Date sent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    /// Email body
    pub body: EmailBody,
    /// Attachments
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Threading information
    #[serde(flatten)]
    pub thread_info: ThreadInfo,
    /// Priority (1 = highest, 5 = lowest)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// All headers (if include_raw_headers is enabled)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, Vec<String>>>,
}

/// Email file format
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum EmailFormat {
    /// RFC 5322 MIME format (.eml)
    Eml,
    /// Outlook MSG format (.msg)
    Msg,
    /// Mbox mailbox format
    Mbox,
}

/// Detect email format from file extension and content
pub fn detect_format(path: &Path) -> Result<EmailFormat> {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        match ext.as_str() {
            "eml" => return Ok(EmailFormat::Eml),
            "msg" => return Ok(EmailFormat::Msg),
            "mbox" => return Ok(EmailFormat::Mbox),
            _ => {}
        }
    }

    // Try to detect from content
    let mut file = File::open(path)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;

    // MSG files start with specific OLE compound file signature
    if header[..8] == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Ok(EmailFormat::Msg);
    }

    // EML files typically start with "Received:", "From:", "Return-Path:", etc.
    let content = std::str::from_utf8(&header).unwrap_or("");
    if content.starts_with("From ") || content.starts_with("Received:") {
        return Ok(EmailFormat::Eml);
    }

    Err(anyhow!(
        "Cannot detect email format for: {}",
        path.display()
    ))
}

/// Parse email from file (auto-detects format)
pub fn parse_email(path: impl AsRef<Path>, options: ParseOptions) -> Result<Email> {
    let path = path.as_ref();
    let format = detect_format(path)?;

    match format {
        EmailFormat::Eml => parse_eml(path, options),
        EmailFormat::Msg => parse_msg(path, options),
        EmailFormat::Mbox => Err(anyhow!("Use parse_mbox for mbox files")),
    }
}

/// Parse an email from bytes, dispatching on an explicit format.
///
/// A caller that already holds the document — an upload, a queue payload —
/// must not have to write it to disk to parse it. Format is explicit rather
/// than sniffed from a file extension, because there is no path to sniff.
pub fn parse_email_bytes(
    bytes: &[u8],
    format: EmailFormat,
    options: ParseOptions,
) -> Result<Email> {
    match format {
        EmailFormat::Eml => parse_eml_bytes(bytes, options),
        EmailFormat::Msg => parse_msg_bytes(bytes, options),
        EmailFormat::Mbox => Err(anyhow!(
            "Mbox requires the streaming parse_mbox API; it is not a single message"
        )),
    }
}

/// Parse EML (RFC 5322 MIME) file
pub fn parse_eml(path: impl AsRef<Path>, options: ParseOptions) -> Result<Email> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("open file: {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file)? };

    parse_eml_bytes(&mmap, options)
}

/// Parse EML from bytes
pub fn parse_eml_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    let parsed =
        mailparse::parse_mail(bytes).map_err(|e| anyhow!("Failed to parse email: {}", e))?;

    // Parse headers
    let headers = parsed.get_headers();

    let from = parse_address_header(&headers, "From");
    let reply_to = parse_address_header(&headers, "Reply-To");
    let to = parse_address_list_header(&headers, "To");
    let cc = parse_address_list_header(&headers, "Cc");
    let bcc = parse_address_list_header(&headers, "Bcc");

    let subject = get_header_value(&headers, "Subject").and_then(|s| decode_header(&s).ok());

    let date = get_header_value(&headers, "Date").and_then(|d| parse_date(&d).ok());

    let message_id = get_header_value(&headers, "Message-ID");
    let in_reply_to = get_header_value(&headers, "In-Reply-To");
    let references = get_header_value(&headers, "References")
        .map(|r| r.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    let priority = get_header_value(&headers, "X-Priority")
        .or_else(|| get_header_value(&headers, "Priority"))
        .and_then(|p| p.parse::<u8>().ok());

    // Parse body
    let mut body = EmailBody::default();
    let mut attachments = Vec::new();

    let mut total_attachment_bytes = 0usize;
    parse_mime_part(
        &parsed,
        &mut body,
        &mut attachments,
        &options,
        &mut total_attachment_bytes,
    )?;

    // Convert HTML to text if requested
    if options.html_to_text && body.text.is_none() {
        if let Some(html) = &body.html {
            body.text = Some(html_to_text(html));
        }
    }

    // Collect raw headers if requested
    let raw_headers = if options.include_raw_headers {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        // mailparse 0.15+: headers is a Vec<MailHeader>, iterate directly
        for header in headers {
            let key = header.get_key().to_string();
            let value = decode_header(&header.get_value()).unwrap_or_default();
            map.entry(key).or_default().push(value);
        }
        Some(map)
    } else {
        None
    };

    Ok(Email {
        message_id: message_id.clone(),
        from: from.unwrap_or_default(),
        reply_to,
        to,
        cc,
        bcc,
        subject,
        date,
        body,
        attachments,
        thread_info: ThreadInfo {
            message_id,
            in_reply_to,
            references,
        },
        priority,
        headers: raw_headers,
    })
}

/// Parse MSG (Outlook) file
pub fn parse_msg(path: impl AsRef<Path>, options: ParseOptions) -> Result<Email> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    parse_msg_bytes(&bytes, options)
}

/// Parse MSG from bytes using OLE CFB property streams (MS-OXMSG).
/// Parse MSG from bytes.
///
/// Exposed for callers that hold an uploaded document rather than a path:
/// the byte entry point is what keeps the parser usable without touching the
/// filesystem.
pub fn parse_msg_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    msg::parse_msg_bytes(bytes, options)
}

/// Parse mbox file (streaming)
pub fn parse_mbox(
    path: impl AsRef<Path>,
    options: ParseOptions,
    mut callback: impl FnMut(Email) -> Result<()>,
) -> Result<usize> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut count = 0;
    let mut current_message = Vec::new();
    let mut in_message = false;

    for line in reader.lines() {
        let line = line?;

        // Mbox messages start with "From " at beginning of line
        if line.starts_with("From ") {
            if !current_message.is_empty() {
                // Parse previous message
                if let Ok(email) = parse_eml_bytes(current_message.join("\n").as_bytes(), options) {
                    callback(email)?;
                    count += 1;
                }
                current_message.clear();
            }
            in_message = true;
            continue;
        }

        if in_message {
            current_message.push(line);
        }
    }

    // Parse last message
    if !current_message.is_empty() {
        if let Ok(email) = parse_eml_bytes(current_message.join("\n").as_bytes(), options) {
            callback(email)?;
            count += 1;
        }
    }

    Ok(count)
}

/// Parse email to NDJSON
pub fn parse_email_to_ndjson(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<()> {
    use std::io::Write;

    let email = parse_email(path, options)?;
    let mut out = std::io::BufWriter::new(File::create(out_path)?);

    serde_json::to_writer(&mut out, &email)?;
    writeln!(out)?;

    Ok(())
}

/// Parse mbox to NDJSON
pub fn parse_mbox_to_ndjson(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<usize> {
    use std::io::Write;

    let mut out = std::io::BufWriter::new(File::create(out_path)?);
    let mut count = 0;

    parse_mbox(path, options, |email| {
        serde_json::to_writer(&mut out, &email)?;
        writeln!(out)?;
        count += 1;
        Ok(())
    })?;

    Ok(count)
}

/// Extract attachments from email
pub fn extract_attachments(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<Vec<PathBuf>> {
    let email = parse_email(path, options)?;
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)?;

    let mut paths = Vec::new();

    for attachment in email.attachments {
        if let Some(content) = attachment.content {
            let out_path = out_dir.join(&attachment.filename);
            use base64::Engine;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&content)
                .map_err(|e| anyhow!("Failed to decode attachment: {}", e))?;
            std::fs::write(&out_path, decoded)?;
            paths.push(out_path);
        }
    }

    Ok(paths)
}

// Helper functions

fn parse_address_header(
    headers: &dyn mailparse::MailHeaderMap,
    name: &str,
) -> Option<EmailAddress> {
    get_header_value(headers, name).and_then(|v| parse_email_address(&v).ok())
}

fn parse_address_list_header(
    headers: &dyn mailparse::MailHeaderMap,
    name: &str,
) -> Vec<EmailAddress> {
    get_header_value(headers, name)
        .map(|v| {
            v.split(',')
                .filter_map(|addr| parse_email_address(addr.trim()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn get_header_value(headers: &dyn mailparse::MailHeaderMap, name: &str) -> Option<String> {
    headers
        .get_first_header(name)
        .map(|h| h.get_value().to_string())
}

fn parse_email_address(s: &str) -> Result<EmailAddress> {
    // Handle formats like:
    // "John Doe" <john@example.com>
    // john@example.com
    // <john@example.com>

    let s = s.trim();

    if let Some(start) = s.find('<') {
        if let Some(end) = s.find('>') {
            let name = s[..start].trim().trim_matches('"').to_string();
            let address = s[start + 1..end].to_string();
            return Ok(EmailAddress {
                name: if name.is_empty() { None } else { Some(name) },
                address,
            });
        }
    }

    // Just an email address
    Ok(EmailAddress {
        name: None,
        address: s.to_string(),
    })
}

fn decode_header(value: &str) -> Result<String> {
    // Handle encoded-words like =?UTF-8?B?...?=
    // Simple decoder for encoded-word syntax: =?charset?encoding?encoded-text?=
    use regex::Regex;

    let re = Regex::new(r"=\?([^?]+)\?([BbQq])\?([^?]+)\?=")
        .expect("static encoded-word regex must compile");
    let result = re.replace_all(value, |caps: &regex::Captures<'_>| {
        let charset = &caps[1];
        let encoding = &caps[2];
        let encoded = &caps[3];

        let decoded = if encoding.eq_ignore_ascii_case("B") {
            // Base64 encoding
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| decode_bytes(&bytes, charset))
        } else if encoding.eq_ignore_ascii_case("Q") {
            // Quoted-printable encoding
            decode_quoted_printable(encoded, charset)
        } else {
            None
        };

        decoded.unwrap_or_else(|| caps[0].to_string())
    });

    Ok(result.to_string())
}

fn decode_bytes(bytes: &[u8], charset: &str) -> Option<String> {
    use encoding_rs::Encoding;

    let encoding = Encoding::for_label(charset.as_bytes())?;
    let (cow, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        None
    } else {
        Some(cow.to_string())
    }
}

fn decode_quoted_printable(encoded: &str, charset: &str) -> Option<String> {
    // Simple quoted-printable decoder
    let mut result = Vec::new();
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '=' {
            if let Some(hex1) = chars.next() {
                if let Some(hex2) = chars.next() {
                    if let Ok(byte) = u8::from_str_radix(&format!("{}{}", hex1, hex2), 16) {
                        result.push(byte);
                    } else {
                        result.push(b'=');
                        result.push(hex1 as u8);
                        result.push(hex2 as u8);
                    }
                } else {
                    result.push(b'=');
                    result.push(hex1 as u8);
                }
            } else {
                result.push(b'=');
            }
        } else if c == '_' {
            result.push(b' ');
        } else {
            result.push(c as u8);
        }
    }

    decode_bytes(&result, charset)
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    // Try various date formats
    // RFC 2822: Mon, 15 Jan 2024 14:30:00 +0000
    // ISO 8601: 2024-01-15T14:30:00Z

    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    Err(anyhow!("Cannot parse date: {}", s))
}

fn parse_mime_part(
    part: &mailparse::ParsedMail<'_>,
    body: &mut EmailBody,
    attachments: &mut Vec<Attachment>,
    options: &ParseOptions,
    total_attachment_bytes: &mut usize,
) -> Result<()> {
    let content_type = part.ctype.mimetype.to_lowercase();
    let is_multipart = content_type.starts_with("multipart/");

    // A multipart/* part is only a container; never treat it as a leaf.
    // Just classify its children.
    if is_multipart {
        for subpart in &part.subparts {
            parse_mime_part(subpart, body, attachments, options, total_attachment_bytes)?;
        }
        return Ok(());
    }

    let content_disposition = part.get_content_disposition();

    // mailparse defaults the disposition to `Inline` when no Content-Disposition
    // header is present, so we must check whether the header actually exists to
    // tell "explicitly inline" apart from "unspecified".
    let has_disposition_header = part
        .headers
        .iter()
        .any(|h| h.get_key().eq_ignore_ascii_case("content-disposition"));
    let explicit_attachment =
        content_disposition.disposition == mailparse::DispositionType::Attachment;
    let explicit_inline = has_disposition_header
        && content_disposition.disposition == mailparse::DispositionType::Inline;

    // Filename may live in the Content-Disposition `filename` param or the
    // Content-Type `name` param (common for attachments with no disposition).
    let filename = content_disposition
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"))
        .cloned();

    let content_id = part
        .headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case("content-id"))
        .map(|h| {
            h.get_value()
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty());

    let is_body_text = content_type == "text/plain" || content_type == "text/html";

    // Inline = embedded resource referenced from the body (cid:) or an
    // explicitly inline disposition. An explicit `attachment` disposition wins.
    let is_inline = explicit_inline || (content_id.is_some() && !explicit_attachment);

    // A leaf is an attachment when it carries an attachment disposition, a
    // filename, or a content-id, or when it is a non-text media type that isn't
    // serving as the message body.
    let is_attachment =
        explicit_attachment || filename.is_some() || content_id.is_some() || !is_body_text;

    if is_attachment {
        let filename = filename.unwrap_or_else(|| fallback_filename(&content_id, &content_type));

        // `raw_bytes` is a borrowed encoded representation, so reject it before
        // asking mailparse to allocate a decoded body. Both ceilings are applied
        // over the running total: max_attachment_size bounds one part,
        // max_total_attachment_bytes bounds their sum.
        let encoded_len = part.raw_bytes.len();
        if encoded_len > options.max_attachment_size
            || encoded_len
                > options
                    .max_total_attachment_bytes
                    .saturating_sub(*total_attachment_bytes)
        {
            return Err(anyhow!("attachment encoded bytes exceed parser budget"));
        }

        let content_data = part
            .get_body_raw()
            .map_err(|e| anyhow!("Failed to read attachment: {}", e))?;
        if content_data.len() > options.max_attachment_size {
            return Err(anyhow!("attachment exceeds max_attachment_size"));
        }
        *total_attachment_bytes = total_attachment_bytes
            .checked_add(content_data.len())
            .ok_or_else(|| anyhow!("attachment byte count overflow"))?;
        if *total_attachment_bytes > options.max_total_attachment_bytes {
            return Err(anyhow!("attachments exceed max_total_attachment_bytes"));
        }

        let attachment = Attachment {
            filename,
            content_type,
            size: content_data.len(),
            content_id,
            is_inline,
            content: if options.extract_attachment_content {
                use base64::Engine;
                Some(base64::engine::general_purpose::STANDARD.encode(&content_data))
            } else {
                None
            },
        };

        attachments.push(attachment);
    } else if content_type == "text/plain" && body.text.is_none() {
        let text = part
            .get_body()
            .map_err(|e| anyhow!("Failed to read text body: {}", e))?;
        body.text = Some(text);
    } else if content_type == "text/html" && body.html.is_none() {
        let html = part
            .get_body()
            .map_err(|e| anyhow!("Failed to read HTML body: {}", e))?;
        body.html = Some(html);
    }

    // Recursively process subparts (e.g. message/rfc822 with nested MIME).
    for subpart in &part.subparts {
        parse_mime_part(subpart, body, attachments, options, total_attachment_bytes)?;
    }

    Ok(())
}

/// Build a filename for an attachment that carries no explicit name,
/// preferring the Content-ID and falling back to a generic name. A
/// file extension is derived from the MIME type so callers writing the
/// attachment to disk get a usable, non-colliding path.
fn fallback_filename(content_id: &Option<String>, content_type: &str) -> String {
    let ext = extension_for_mime(content_type);
    let stem = content_id
        .as_deref()
        .map(sanitize_filename_component)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unnamed".to_string());

    if stem.contains('.') || ext.is_empty() {
        stem
    } else {
        format!("{stem}{ext}")
    }
}

/// Replace characters that are unsafe in a filesystem path so a Content-ID
/// (which may contain `@`, `/`, etc.) can be reused as a filename component.
fn sanitize_filename_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Map a MIME type to a common file extension (including the leading dot),
/// or an empty string when unknown.
fn extension_for_mime(content_type: &str) -> &'static str {
    match content_type.split(';').next().unwrap_or("").trim() {
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "image/tiff" => ".tiff",
        "image/svg+xml" => ".svg",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "application/json" => ".json",
        "application/msword" => ".doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => ".docx",
        "application/vnd.ms-excel" => ".xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/csv" => ".csv",
        "message/rfc822" => ".eml",
        _ => "",
    }
}

fn html_to_text(html: &str) -> String {
    // Simple HTML to text conversion
    // A full implementation would use a proper HTML parser
    let text = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<p>", "\n")
        .replace("</p>", "\n");

    // Remove remaining HTML tags
    let re = regex::Regex::new(r"<[^>]+>").expect("static HTML tag regex must compile");
    re.replace_all(&text, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_parse_email_address() {
        let addr = parse_email_address("john@example.com").unwrap();
        assert_eq!(addr.address, "john@example.com");
        assert_eq!(addr.name, None);

        let addr = parse_email_address("\"John Doe\" <john@example.com>").unwrap();
        assert_eq!(addr.address, "john@example.com");
        assert_eq!(addr.name, Some("John Doe".to_string()));
    }

    #[test]
    fn parse_eml_bytes_decodes_headers_and_html_body() {
        let raw = concat!(
            "From: \"Maria\" <maria@example.com>\r\n",
            "To: joao@example.com\r\n",
            "Subject: =?UTF-8?Q?Ol=C3=A1?=\r\n",
            "Date: Mon, 15 Jan 2024 14:30:00 +0000\r\n",
            "Message-ID: <msg-1@example.com>\r\n",
            "Content-Type: text/html; charset=UTF-8\r\n",
            "\r\n",
            "<p>Ol\u{00e1} mundo</p>\r\n"
        );

        let email = parse_eml_bytes(
            raw.as_bytes(),
            ParseOptions {
                html_to_text: true,
                ..ParseOptions::default()
            },
        )
        .unwrap();

        assert_eq!(email.from.address, "maria@example.com");
        assert_eq!(email.subject.as_deref(), Some("Olá"));
        assert!(email
            .body
            .html
            .as_deref()
            .unwrap_or_default()
            .contains("Olá mundo"));
        assert!(email
            .body
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("Olá mundo"));
        assert_eq!(email.message_id.as_deref(), Some("<msg-1@example.com>"));
    }

    #[test]
    fn parse_msg_bytes_extracts_subject_sender_and_body() {
        // WHY: MSG must come from CFB property streams, not a whole-file
        // string search that can return the OLE bytes as the body.
        let bytes = write_msg_cfb_fixture(
            "Quarterly update",
            "sender@example.com",
            "recipient@example.com",
            "Mensagem MSG",
        );

        let email = parse_msg_bytes(&bytes, ParseOptions::default()).unwrap();

        assert_eq!(email.subject.as_deref(), Some("Quarterly update"));
        assert_eq!(email.from.address, "sender@example.com");
        assert_eq!(email.to.len(), 1);
        assert_eq!(email.to[0].address, "recipient@example.com");
        assert_eq!(email.body.text.as_deref(), Some("Mensagem MSG"));
    }

    #[test]
    fn parse_msg_bytes_rejects_ole_without_property_streams() {
        let mut bytes = vec![0; 512];
        bytes[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
        bytes.extend_from_slice(b"this is not a message object");
        let err = parse_msg_bytes(&bytes, ParseOptions::default()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CFB") || msg.contains("substg") || msg.contains("valid MSG"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn attachment_name_in_content_type_without_disposition() {
        // Many clients (and message/* parts) omit Content-Disposition and put the
        // filename only in the Content-Type `name` param. This must still be
        // captured as a non-inline attachment, not silently dropped.
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: Bare attachment\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain; charset=UTF-8\r\n",
            "\r\n",
            "Body text\r\n",
            "--b\r\n",
            "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--b--\r\n"
        );

        let email = parse_eml_bytes(raw.as_bytes(), ParseOptions::default()).unwrap();

        assert_eq!(email.body.text.as_deref().map(str::trim), Some("Body text"));
        assert_eq!(email.attachments.len(), 1);
        let att = &email.attachments[0];
        assert_eq!(att.filename, "report.pdf");
        assert_eq!(att.content_type, "application/pdf");
        assert!(!att.is_inline);
        assert_eq!(att.content_id, None);
    }

    #[test]
    fn inline_image_with_only_content_id_is_inline() {
        // multipart/related inline image referenced by cid: — no
        // Content-Disposition, no filename, only a Content-ID. Must be captured,
        // marked inline, and given a usable fallback filename.
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: Inline image\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/related; boundary=\"r\"\r\n",
            "\r\n",
            "--r\r\n",
            "Content-Type: text/html; charset=UTF-8\r\n",
            "\r\n",
            "<p>See <img src=\"cid:logo@x\"></p>\r\n",
            "--r\r\n",
            "Content-Type: image/png\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-ID: <logo@x>\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--r--\r\n"
        );

        let email = parse_eml_bytes(raw.as_bytes(), ParseOptions::default()).unwrap();

        assert!(email
            .body
            .html
            .as_deref()
            .unwrap_or_default()
            .contains("cid:logo@x"));
        assert_eq!(email.attachments.len(), 1);
        let att = &email.attachments[0];
        assert!(att.is_inline);
        assert_eq!(att.content_id.as_deref(), Some("logo@x"));
        assert_eq!(att.content_type, "image/png");
        // Content-ID reused as filename stem with a derived extension.
        assert_eq!(att.filename, "logo@x.png");
    }

    #[test]
    fn binary_attachment_without_name_gets_fallback_filename() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: Unnamed binary\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "Hi\r\n",
            "--b\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--b--\r\n"
        );

        let email = parse_eml_bytes(raw.as_bytes(), ParseOptions::default()).unwrap();

        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].filename, "unnamed");
        assert!(!email.attachments[0].is_inline);
    }

    #[test]
    fn parse_mbox_splits_multiple_messages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mailbox.mbox");
        fs::write(
            &path,
            concat!(
                "From sender@example.com Mon Jan 15 14:30:00 2024\n",
                "From: sender@example.com\n",
                "To: first@example.com\n",
                "Subject: First\n",
                "\n",
                "Primeira mensagem\n",
                "From other@example.com Tue Jan 16 08:00:00 2024\n",
                "From: other@example.com\n",
                "To: second@example.com\n",
                "Subject: Second\n",
                "\n",
                "Segunda mensagem\n"
            ),
        )
        .unwrap();

        let mut subjects = Vec::new();
        let count = parse_mbox(&path, ParseOptions::default(), |email| {
            subjects.push(email.subject.unwrap_or_default());
            Ok(())
        })
        .unwrap();

        assert_eq!(count, 2);
        assert_eq!(subjects, vec!["First", "Second"]);
    }
}
