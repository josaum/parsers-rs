#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # email-fast-core
//!
//! High-performance email parser for .eml and .msg formats.
//!
//! ## Features
//!
//! - **Fast**: Memory-mapped file access for large mailboxes
//! - **Projection**: Headers, body (text/html), attachments, threading
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

use anyhow::{anyhow, ensure, Context, Result};
use chrono::{DateTime, Utc};
use memmap2::Mmap;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(feature = "arrow")]
pub mod arrow;

// The GLiNER extractor and the API embedder that were declared here are
// consumer integrations: they depend on one product's SDK and belong in that
// product, not in a parser shared across consumers.

mod extraction;
mod mbox;
mod mime;
mod msg;

/// Hidden CFB fixture writer for integration tests (mini-stream MSG layout).
#[doc(hidden)]
pub fn write_msg_cfb_fixture(subject: &str, sender: &str, to: &str, body: &str) -> Vec<u8> {
    msg::write_fixture_msg(subject, sender, to, body)
}

/// Configuration options for email parsing.
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Maximum input message bytes and aggregate decoded content bytes.
    /// Each ceiling is checked separately; this is not a process memory cap.
    pub max_inflate: usize,
    /// Maximum MIME/MSG nesting depth (default: 64).
    pub max_mime_depth: usize,
    /// Maximum MIME parts or MSG directory entries (default: 10,000).
    pub max_mime_parts: usize,
    /// Maximum header bytes per MIME part (default: 256 KiB).
    pub max_header_bytes: usize,
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
            max_mime_depth: 64,
            max_mime_parts: 10_000,
            max_header_bytes: 256 * 1024,
            max_inflate: 128 * 1024 * 1024,                // 128 MiB
            max_attachment_size: 100 * 1024 * 1024,        // 100 MB
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

impl Attachment {
    /// The attachment's file extension, lowercased, if any.
    ///
    /// Consumers classifying attachments (`attach|pdf`, `attach|zip`)
    /// should use this instead of reimplementing `rsplit('.')`.
    pub fn extension(&self) -> Option<String> {
        let idx = self.filename.rfind('.')?;
        Some(self.filename[idx + 1..].to_ascii_lowercase())
    }

    /// Whether this attachment is itself an email (message/rfc822, or an
    /// `.eml` file), i.e. a candidate for recursive parsing.
    pub fn is_nested_message(&self) -> bool {
        self.content_type
            .to_ascii_lowercase()
            .contains("message/rfc822")
            || self
                .content_type
                .eq_ignore_ascii_case("application/vnd.ms-outlook")
            || self.filename.to_ascii_lowercase().ends_with(".eml")
            || self.filename.to_ascii_lowercase().ends_with(".msg")
    }

    /// Decoded attachment bytes, when content extraction was enabled
    /// (`ParseOptions::extract_attachment_content`). Content is
    /// base64-encoded in transit; this decodes it.
    pub fn decode_content(&self) -> Option<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(self.content.as_deref()?)
            .ok()
    }
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
    /// Message ID, also available on Email; omitted here to avoid duplicate JSON keys.
    #[serde(skip_serializing)]
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

impl Email {
    /// Whether the parse produced anything a consumer could use: a sender
    /// address, a subject, body text/html, or at least one attachment.
    ///
    /// The parser is deliberately lenient — arbitrary bytes "parse" into
    /// an empty Email rather than erroring, so streaming consumers never
    /// lose a whole mbox to one bad message. Strict consumers should call
    /// this to reject degenerate results instead of hand-rolling the
    /// check (a from-less, subject-less, body-less, attachment-less
    /// "email" is garbage in, garbage out).
    pub fn has_substantive_content(&self) -> bool {
        !self.from.address.trim().is_empty()
            || self
                .subject
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            || self
                .body
                .text
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty())
            || self
                .body
                .html
                .as_deref()
                .is_some_and(|h| !h.trim().is_empty())
            || !self.attachments.is_empty()
    }
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
    ensure!(
        file.metadata()?.len() <= options.max_inflate as u64,
        "message exceeds max_inflate"
    );
    if file.metadata()?.len() == 0 {
        return parse_eml_bytes(&[], options);
    }
    let mmap = unsafe { Mmap::map(&file)? };

    parse_eml_bytes(&mmap, options)
}

/// Parse EML from bytes
pub fn parse_eml_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    parse_eml_with_sink(bytes, options, None)
}

fn parse_eml_with_sink(
    bytes: &[u8],
    options: ParseOptions,
    sink: Option<&mut mime::AttachmentSink<'_>>,
) -> Result<Email> {
    ensure!(
        bytes.len() <= options.max_inflate,
        "message exceeds max_inflate"
    );
    mime::check_root_headers(bytes, options.max_header_bytes)?;
    let parsed =
        mailparse::parse_mail(bytes).map_err(|e| anyhow!("Failed to parse email: {}", e))?;

    // Parse headers
    mime::validate_tree(&parsed, &options)?;
    let headers = parsed.headers.as_slice();

    let from = parse_address_header(headers, "From");
    let reply_to = parse_address_header(headers, "Reply-To");
    let to = parse_address_list_header(headers, "To");
    let cc = parse_address_list_header(headers, "Cc");
    let bcc = parse_address_list_header(headers, "Bcc");

    let subject = get_header_value(headers, "Subject");

    let date = get_header_value(headers, "Date").and_then(|d| parse_date(&d).ok());

    let message_id = get_header_value(headers, "Message-ID");
    let in_reply_to = get_header_value(headers, "In-Reply-To");
    let references = get_header_value(headers, "References")
        .map(|r| r.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    let priority = get_header_value(headers, "X-Priority")
        .or_else(|| get_header_value(headers, "Priority"))
        .and_then(|p| p.parse::<u8>().ok());

    // Parse body
    let (mut body, attachments) = mime::parse(&parsed, options, sink)?;

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
            let value = header.get_value();
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
    let bytes = read_bounded(path, options.max_inflate)?;
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

/// Parse an mboxrd/mboxo mailbox without changing MIME bytes. Errors include offsets.
/// Stops on the first parse failure. Use [`parse_mbox_with_errors`] to continue explicitly.
pub fn parse_mbox(
    path: impl AsRef<Path>,
    options: ParseOptions,
    callback: impl FnMut(Email) -> Result<()>,
) -> Result<usize> {
    mbox::parse(path.as_ref(), options, callback, |_, _, error| Err(error))
}

/// Parse an mbox with an explicit recovery policy. The error callback receives
/// the one-based message index and byte offset; returning Ok skips that message.
/// Callback/output errors always propagate and are never treated as parse errors.
pub fn parse_mbox_with_errors(
    path: impl AsRef<Path>,
    options: ParseOptions,
    callback: impl FnMut(Email) -> Result<()>,
    on_error: impl FnMut(usize, u64, anyhow::Error) -> Result<()>,
) -> Result<usize> {
    mbox::parse(path.as_ref(), options, callback, on_error)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "message exceeds max_inflate"
    );
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "message exceeds max_inflate");
    Ok(bytes)
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
    out.flush()?;

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
    out.flush()?;

    Ok(count)
}

/// Extract attachments with bounded decoding and exclusive file creation.
/// Sender-supplied directory components are removed; collisions receive numeric
/// suffixes. Existing files/symlinks are not overwritten. Completed attachments
/// may remain if a later part fails. Content extraction is enabled automatically.
pub fn extract_attachments(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<Vec<PathBuf>> {
    extraction::extract(path.as_ref(), out_dir.as_ref(), options)
}

// Helper functions

fn parse_address_header(
    headers: &(impl mailparse::MailHeaderMap + ?Sized),
    name: &str,
) -> Option<EmailAddress> {
    parse_address_list_header(headers, name).into_iter().next()
}

fn parse_address_list_header(
    headers: &(impl mailparse::MailHeaderMap + ?Sized),
    name: &str,
) -> Vec<EmailAddress> {
    headers
        .get_all_headers(name)
        .into_iter()
        .filter_map(|header| mailparse::addrparse_header(header).ok())
        .flat_map(|list| flatten_addresses(&list))
        .collect()
}

fn flatten_addresses(list: &mailparse::MailAddrList) -> Vec<EmailAddress> {
    let convert = |info: &mailparse::SingleInfo| EmailAddress {
        name: info.display_name.clone(),
        address: info.addr.clone(),
    };
    list.iter()
        .flat_map(|addr| match addr {
            mailparse::MailAddr::Single(info) => vec![convert(info)],
            mailparse::MailAddr::Group(group) => group.addrs.iter().map(convert).collect(),
        })
        .collect()
}

fn get_header_value(
    headers: &(impl mailparse::MailHeaderMap + ?Sized),
    name: &str,
) -> Option<String> {
    headers.get_first_header(name).map(|h| h.get_value())
}

#[cfg(test)]
fn parse_email_address(s: &str) -> Result<EmailAddress> {
    flatten_addresses(&mailparse::addrparse(s)?)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("empty address"))
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
    static TAGS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").expect("static HTML tag regex"));
    TAGS.replace_all(&text, "").to_string()
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

    #[test]
    fn attachment_helpers_classify_correctly() {
        let att = Attachment {
            filename: "Report.PDF".into(),
            content_type: "application/pdf".into(),
            size: 4,
            content_id: None,
            is_inline: false,
            content: None,
        };
        assert_eq!(att.extension().as_deref(), Some("pdf"));
        assert!(!att.is_nested_message());
        assert_eq!(att.decode_content(), None);

        let nested = Attachment {
            filename: "thread.eml".into(),
            content_type: "message/rfc822".into(),
            size: 3,
            content_id: None,
            is_inline: false,
            content: Some("SGVsbG8=".to_string()), // "Hello"
        };
        assert!(nested.is_nested_message());
        assert_eq!(
            nested.decode_content().as_deref(),
            Some(b"Hello".as_slice())
        );

        let no_ext = Attachment {
            filename: "README".into(),
            content_type: "text/plain".into(),
            size: 0,
            content_id: None,
            is_inline: false,
            content: None,
        };
        assert_eq!(no_ext.extension(), None);
    }

    #[test]
    fn degenerate_email_has_no_substantive_content() {
        // The parser accepts arbitrary bytes; the strict-consumer check
        // must reject a nothing-there result.
        fn bare_email() -> Email {
            Email {
                message_id: None,
                from: EmailAddress {
                    name: None,
                    address: String::new(),
                },
                reply_to: None,
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: None,
                date: None,
                body: EmailBody::default(),
                attachments: Vec::new(),
                thread_info: ThreadInfo::default(),
                priority: None,
                headers: None,
            }
        }
        let junk = bare_email();
        assert!(!junk.has_substantive_content());

        let mut with_subject = bare_email();
        with_subject.subject = Some("hello".into());
        assert!(with_subject.has_substantive_content());

        let mut with_attachment = bare_email();
        with_attachment.attachments.push(Attachment {
            filename: "a.zip".into(),
            content_type: "application/zip".into(),
            size: 2,
            content_id: None,
            is_inline: false,
            content: None,
        });
        assert!(with_attachment.has_substantive_content());
    }

    #[test]
    fn mbox_with_invalid_utf8_bytes_survives() {
        // Real-world mbox corpora carry non-UTF-8 bytes (1990s mail,
        // latin-1 bodies). One bad byte must not abort the mailbox.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bad.mbox");
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(
            b"From a@x.example Mon Jan 1 00:00:00 2024\nFrom: a@x.example\nTo: b@y.example\nSubject: first\n\nbody one\n",
        );
        bytes.extend_from_slice(&[0xFF, 0xFE, b'\n']); // invalid UTF-8 line
        bytes.extend_from_slice(
            b"From c@z.example Mon Jan 2 00:00:00 2024\nFrom: c@z.example\nTo: b@y.example\nSubject: second\n\nbody two\n",
        );
        std::fs::write(&path, bytes).unwrap();
        let mut subjects = Vec::new();
        let count = parse_mbox(&path, ParseOptions::default(), |email| {
            subjects.push(email.subject.unwrap_or_default());
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2, "both messages must survive: {subjects:?}");
        assert!(subjects.contains(&"first".to_string()));
        assert!(subjects.contains(&"second".to_string()));
    }
}
