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

// The GLiNER extractor and the API embedder that were declared here are
// consumer integrations: they depend on one product's SDK and belong in that
// product, not in a parser shared across consumers.

mod msg;

/// The shared mutation harness, for unit tests that compare private helpers
/// against `mailparse` over mutated input.
#[cfg(test)]
#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

/// Hidden CFB fixture writer for integration tests (mini-stream MSG layout).
#[doc(hidden)]
pub fn write_msg_cfb_fixture(subject: &str, sender: &str, to: &str, body: &str) -> Vec<u8> {
    msg::write_fixture_msg(subject, sender, to, body)
}

/// Hidden CFB fixture writer for integration tests: arbitrary named streams.
///
/// A name of the form `storage/stream` puts the stream in a storage under the
/// root, which is how attachment and recipient storages are written.
#[doc(hidden)]
pub fn write_msg_cfb_fixture_streams(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    msg::write_fixture_streams(streams)
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
            || self.filename.to_ascii_lowercase().ends_with(".eml")
    }

    /// Decoded attachment bytes, when content extraction was enabled
    /// (`ParseOptions::extract_attachment_content`). Content is
    /// base64-encoded in transit; this decodes it.
    pub fn decode_content(&self) -> Option<Vec<u8>> {
        let content = self.content.as_deref()?;
        let mut table = [255u8; 256];
        for (i, c) in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
            .bytes()
            .enumerate()
        {
            table[c as usize] = i as u8;
        }
        let mut out = Vec::with_capacity(content.len() / 4 * 3);
        let (mut acc, mut bits) = (0u32, 0u32);
        for &b in content.as_bytes() {
            let v = table[b as usize];
            if v == 255 {
                continue;
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
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
    let mmap = unsafe { Mmap::map(&file)? };

    parse_eml_bytes(&mmap, options)
}

/// Parse EML from bytes
pub fn parse_eml_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    // `mailparse` recurses once per nested multipart, so the depth is checked
    // before it sees the message.
    if mime_nesting_exceeds(bytes, MAX_MIME_DEPTH) {
        return Err(anyhow!(
            "Failed to parse email: multipart nesting deeper than {MAX_MIME_DEPTH} levels"
        ));
    }
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
    callback: impl FnMut(Email) -> Result<()>,
) -> Result<usize> {
    let file = File::open(path.as_ref())?;
    parse_mbox_reader(BufReader::new(file), options, callback)
}

/// Parse an mbox mailbox from a buffered reader (streaming).
///
/// The reader form of [`parse_mbox`], for a caller that holds the mailbox in
/// memory or receives it from a stream. `callback` runs once per message, in
/// mailbox order; the return value is the number of messages delivered.
///
/// # Errors
///
/// Returns an error if the reader fails or `callback` returns one. A message
/// that does not parse is skipped, so one bad message does not lose the
/// mailbox.
pub fn parse_mbox_reader(
    mut reader: impl BufRead,
    options: ParseOptions,
    mut callback: impl FnMut(Email) -> Result<()>,
) -> Result<usize> {
    let mut count = 0;
    let mut current_message: Vec<u8> = Vec::new();
    let mut in_message = false;

    // Byte-wise line reads: real-world mbox corpora (e.g. 1990s corporate
    // mail) contain non-UTF-8 bytes, so `BufRead::lines` would abort the whole
    // mailbox on the first one. The bytes are kept exactly as stored rather
    // than converted lossily, because an 8-bit body is only decodable through
    // the charset its own headers declare; replacing its bytes here would
    // leave U+FFFD for the MIME decoder to misread as that charset.
    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let mut line = raw.as_slice();
        while let [rest @ .., b'\r' | b'\n'] = line {
            line = rest;
        }

        // A message starts at a "From " separator line. The first one of the
        // mailbox is taken as written; after that the line must carry the
        // separator's date, or body text beginning with the word "From"
        // would cut the message in two.
        if line.starts_with(b"From ") && (!in_message || is_mbox_separator(line)) {
            if !current_message.is_empty() {
                // Parse previous message
                if let Ok(email) = parse_eml_bytes(&current_message, options) {
                    callback(email)?;
                    count += 1;
                }
                current_message.clear();
            }
            in_message = true;
            continue;
        }

        if in_message {
            if !current_message.is_empty() {
                current_message.push(b'\n');
            }
            current_message.extend_from_slice(line);
        }
    }

    // Parse last message
    if !current_message.is_empty() {
        if let Ok(email) = parse_eml_bytes(&current_message, options) {
            callback(email)?;
            count += 1;
        }
    }

    Ok(count)
}

/// Reports whether `line` is an mbox message separator (a "From_ line").
///
/// A separator is `From `, an envelope sender, and a date in `asctime` form:
/// weekday, month, day of month, time, and a four-digit year, with an
/// optional time zone before or after the year. All of these are accepted:
///
/// ```text
/// From a@example.com Mon Jan  1 00:00:00 2024
/// From MAILER-DAEMON Mon Jan 15 14:30 2024
/// From - Mon Jan 15 14:30:00 2024
/// From 1234@xxx Mon Jan 15 14:30:00 +0000 2024
/// ```
///
/// The sender may be missing or contain spaces, so the date is looked for
/// anywhere in the line's first few words. A body line such as
/// `From now on, ...` has no such date and stays in its message.
fn is_mbox_separator(line: &[u8]) -> bool {
    /// Words examined after `From `. A separator is a sender plus at most
    /// seven date words; the rest of a long body line is not worth scanning.
    const MAX_WORDS: usize = 16;
    /// Words after the time that may hold the year: a zone on either side.
    const YEAR_WINDOW: usize = 3;

    fn is_name(word: &[u8], names: &[&str]) -> bool {
        names
            .iter()
            .any(|name| word.eq_ignore_ascii_case(name.as_bytes()))
    }
    fn is_number(word: &[u8], digits: std::ops::RangeInclusive<usize>) -> bool {
        digits.contains(&word.len()) && word.iter().all(u8::is_ascii_digit)
    }
    fn is_time(word: &[u8]) -> bool {
        let mut fields = 0;
        word.split(|byte| *byte == b':').all(|field| {
            fields += 1;
            is_number(field, 1..=2)
        }) && (2..=3).contains(&fields)
    }

    let Some(rest) = line.strip_prefix(b"From ") else {
        return false;
    };
    let words: Vec<&[u8]> = rest
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty())
        .take(MAX_WORDS)
        .collect();
    words.windows(4).enumerate().any(|(at, date)| {
        is_name(date[0], &["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"])
            && is_name(
                date[1],
                &[
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ],
            )
            && is_number(date[2], 1..=2)
            && is_time(date[3])
            && words[at + 4..]
                .iter()
                .take(YEAR_WINDOW)
                .any(|word| is_number(word, 4..=4))
    })
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

/// Deepest nesting of multipart containers a message may have.
///
/// `mailparse` builds the MIME tree by recursion, and this crate walks and
/// drops that tree the same way. An unoptimized build spends about 2.5 KiB of
/// stack per level: a 256 KiB stack overflows near 105 levels. Thirty-two
/// levels keep the whole parse within a third of such a stack, and mail that
/// nests past ten levels is not something a mail client produces.
const MAX_MIME_DEPTH: usize = 32;

/// Reports whether `raw` nests multipart containers more than `limit` deep.
///
/// A message of ordinary shape is answered by counting lines: every level of
/// nesting is opened by its own boundary delimiter line, and a delimiter line
/// starts with `--`, so a message with at most `limit` such lines cannot nest
/// deeper than `limit`. Only a message with more of them is walked.
fn mime_nesting_exceeds(raw: &[u8], limit: usize) -> bool {
    let mut dash_lines = usize::from(raw.starts_with(b"--"));
    for window in raw.windows(3) {
        if window == b"\n--" {
            dash_lines += 1;
            if dash_lines > limit {
                return mime_nesting_depth(raw, limit).is_none();
            }
        }
    }
    false
}

/// Returns how deep multipart containers nest in `raw`, or `None` once the
/// depth passes `limit`.
///
/// The depth is that of the deepest part: how many containers enclose it. A
/// message that is a single part has depth 0. The walk splits parts exactly
/// as `mailparse::parse_mail` (version 0.15) does, with the same header
/// parser and the same boundary search, but keeps its work list on the heap,
/// so it can measure a tree that would overflow the stack if `mailparse`
/// were handed it. It must agree with `mailparse` on every input;
/// `mime_depth_walk_agrees_with_mailparse` checks that.
fn mime_nesting_depth(raw: &[u8], limit: usize) -> Option<usize> {
    let mut deepest = 0;
    let mut pending: Vec<(&[u8], usize)> = vec![(raw, 0)];
    while let Some((part, depth)) = pending.pop() {
        // A part whose headers do not parse fails the real parse as well, but
        // possibly only after `mailparse` has descended into earlier parts,
        // so the remaining parts are still measured.
        let Ok((headers, body_start)) = mailparse::parse_headers(part) else {
            continue;
        };
        let Some(content_type) =
            mailparse::MailHeaderMap::get_first_value(headers.as_slice(), "Content-Type")
        else {
            continue;
        };
        let content_type = mailparse::parse_content_type(&content_type);
        let Some(boundary) = content_type.params.get("boundary") else {
            continue;
        };
        if !content_type.mimetype.starts_with("multipart/") || part.len() <= body_start {
            continue;
        }
        let delimiter = format!("--{boundary}");
        let delimiter = delimiter.as_bytes();
        let Some(first_delimiter) = find_line_prefix(part, body_start, delimiter) else {
            continue;
        };
        let child_depth = depth + 1;
        let mut delimiter_end = first_delimiter + delimiter.len();
        while let Some(part_start) = find_bytes(part, delimiter_end, b"\n").map(|at| at + 1) {
            if child_depth > limit {
                return None;
            }
            deepest = deepest.max(child_depth);
            let part_end = find_line_prefix(part, part_start, delimiter).unwrap_or(part.len());
            pending.push((&part[part_start..part_end], child_depth));
            delimiter_end = part_end + delimiter.len();
            if delimiter_end + 2 > part.len() || part[delimiter_end..].starts_with(b"--") {
                break;
            }
        }
    }
    Some(deepest)
}

/// Finds `key` in `data` at or after `start`, as `mailparse` does.
///
/// This reproduces one quirk of the `mailparse` search on purpose: a match
/// that ends exactly at the end of `data` is not reported. The depth walk has
/// to split parts where `mailparse` splits them, quirks included.
fn find_bytes(data: &[u8], start: usize, key: &[u8]) -> Option<usize> {
    // Candidate positions are `start..last`; the final one is left out.
    let last = data.len().checked_sub(key.len())?;
    if key.is_empty() || start >= last {
        return None;
    }
    data[start..data.len() - 1]
        .windows(key.len())
        .position(|window| window == key)
        .map(|at| start + at)
}

/// Finds `key` at `start` or at the start of a later line, as `mailparse` does.
fn find_line_prefix(data: &[u8], start: usize, key: &[u8]) -> Option<usize> {
    let mut from = start;
    while let Some(at) = find_bytes(data, from, key) {
        if at == start || data[at - 1] == b'\n' {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn parse_address_header(
    headers: &dyn mailparse::MailHeaderMap,
    name: &str,
) -> Option<EmailAddress> {
    let header = headers.get_first_header(name)?;
    if let Some(first) = structured_addresses(header).and_then(|list| list.into_iter().next()) {
        return Some(first);
    }
    parse_email_address(&header.get_value()).ok()
}

fn parse_address_list_header(
    headers: &dyn mailparse::MailHeaderMap,
    name: &str,
) -> Vec<EmailAddress> {
    let Some(header) = headers.get_first_header(name) else {
        return Vec::new();
    };
    if let Some(list) = structured_addresses(header) {
        return list;
    }
    // Not RFC 5322 address syntax; keep whatever the sender wrote rather than
    // dropping the recipients.
    header
        .get_value()
        .split(',')
        .filter_map(|addr| parse_email_address(addr.trim()).ok())
        .collect()
}

/// Parses an address header with the RFC 5322 grammar.
///
/// A comma separates mailboxes only outside a quoted string and outside an
/// encoded-word, so `"Doe, Jane" <j@example.com>` and a display name whose
/// encoded form decodes to text containing a comma each stay one mailbox.
/// Group members (`Team: a@x, b@y;`) are flattened into the list. Returns
/// `None` when the header does not parse or names no mailbox, so the caller
/// can fall back to lenient splitting.
fn structured_addresses(header: &mailparse::MailHeader<'_>) -> Option<Vec<EmailAddress>> {
    let parsed = mailparse::addrparse_header(header).ok()?;
    let mut addresses = Vec::new();
    for entry in parsed.iter() {
        match entry {
            mailparse::MailAddr::Single(single) => addresses.push(email_address_from(single)),
            mailparse::MailAddr::Group(group) => {
                addresses.extend(group.addrs.iter().map(email_address_from));
            }
        }
    }
    if addresses.is_empty() {
        None
    } else {
        Some(addresses)
    }
}

fn email_address_from(single: &mailparse::SingleInfo) -> EmailAddress {
    EmailAddress {
        name: single
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        // `mailparse` leaves the separator on a mailbox that follows a group
        // (`Team: a@x;, c@y` yields `, c@y`); no address starts with one.
        address: single
            .addr
            .trim_start_matches(|ch: char| ch == ',' || ch.is_whitespace())
            .to_string(),
    }
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
        // The closing bracket is looked for after the opening one. Two
        // independent searches give an inverted range for `a> <b`.
        let rest = &s[start + 1..];
        if let Some(len) = rest.find('>') {
            let name = s[..start].trim().trim_matches('"').to_string();
            let address = rest[..len].to_string();
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
    use std::sync::LazyLock;

    // Compiled once: this runs for every header of every message, and
    // building the automaton costs far more than matching one header.
    static ENCODED_WORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"=\?([^?]+)\?([BbQq])\?([^?]+)\?=")
            .expect("static encoded-word regex must compile")
    });
    let result = ENCODED_WORD.replace_all(value, |caps: &regex::Captures<'_>| {
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

    let Some(encoding) = Encoding::for_label(charset.as_bytes()) else {
        return Some(decode_unlabeled_bytes(bytes));
    };
    let (cow, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        None
    } else {
        Some(cow.to_string())
    }
}

/// Decodes text whose charset label is missing or names no known encoding.
///
/// Valid UTF-8 is taken as UTF-8. Anything else is read as windows-1252, the
/// encoding mail clients themselves assume for unlabeled 8-bit text: it maps
/// every byte, so no character is lost to U+FFFD.
pub(crate) fn decode_unlabeled_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
    }
}

/// Decodes a `text/*` part body through the charset its headers declare.
///
/// `mailparse` substitutes U+FFFD for every non-ASCII byte when the label is
/// one it does not recognize (for example `x-unknown`), which loses the text.
/// Those parts are decoded from their transfer-decoded bytes instead.
fn decode_text_body(part: &mailparse::ParsedMail<'_>) -> Result<String> {
    if encoding_rs::Encoding::for_label(part.ctype.charset.as_bytes()).is_some() {
        return part.get_body().map_err(|e| anyhow!("{e}"));
    }
    let raw = part.get_body_raw().map_err(|e| anyhow!("{e}"))?;
    Ok(decode_unlabeled_bytes(&raw))
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
        let text =
            decode_text_body(part).map_err(|e| anyhow!("Failed to read text body: {}", e))?;
        body.text = Some(text);
    } else if content_type == "text/html" && body.html.is_none() {
        let html =
            decode_text_body(part).map_err(|e| anyhow!("Failed to read HTML body: {}", e))?;
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

/// Elements whose content is code or style rules, never text to show.
const RAW_TEXT_ELEMENTS: [&str; 2] = ["script", "style"];

/// Converts an HTML body to plain text.
///
/// Tags are removed, the content of `<script>` and `<style>` elements and of
/// comments is dropped, `<br>`, `<br/>`, `<p>` and `</p>` become line breaks,
/// and character references are decoded. This is a single pass over the
/// text, not an HTML parser: it does not build a tree or lay out blocks.
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    // Set once a search for `>` has failed: no later tag can be closed
    // either, and searching again for each `<` would cost quadratic time.
    let mut no_closing_bracket = false;
    while let Some(open) = rest.find('<') {
        text.push_str(&rest[..open]);
        let tag = &rest[open..];

        if let Some(comment) = tag.strip_prefix("<!--") {
            // `<!-->` and `<!--->` are empty comments; any other comment runs
            // to `-->`, or to the end of the text when it is never closed.
            rest = if let Some(after) = comment.strip_prefix('>') {
                after
            } else if let Some(after) = comment.strip_prefix("->") {
                after
            } else {
                comment
                    .find("-->")
                    .map_or("", |end| &comment[end + "-->".len()..])
            };
            continue;
        }

        if let Some(name) = raw_text_element_name(tag) {
            // The element's content is skipped up to its end tag. With no end
            // tag (or a start tag that never closes) the rest is all content.
            rest = tag
                .find('>')
                .and_then(|start_tag_end| skip_past_end_tag(&tag[start_tag_end + 1..], name))
                .unwrap_or("");
            continue;
        }

        // Markup has a letter, `/`, `!` or `?` right after the `<`; any other
        // `<` (as in `a < b`) is text.
        if !tag[1..]
            .starts_with(|ch: char| ch.is_ascii_alphabetic() || matches!(ch, '/' | '!' | '?'))
        {
            text.push('<');
            rest = &tag[1..];
            continue;
        }

        let closing = if no_closing_bracket {
            None
        } else {
            tag.find('>')
        };
        match closing {
            // `<` with nothing to close it, or the empty `<>`, is text.
            None | Some(1) => {
                no_closing_bracket |= closing.is_none();
                text.push('<');
                rest = &tag[1..];
            }
            Some(end) => {
                if matches!(&tag[1..end], "br" | "br/" | "p" | "/p") {
                    text.push('\n');
                }
                rest = &tag[end + 1..];
            }
        }
    }
    text.push_str(rest);
    // Character references are decoded only after the tags are gone, so an
    // escaped `&lt;b&gt;` survives as the literal text `<b>` instead of being
    // stripped as markup.
    decode_html_entities(&text)
}

/// Returns the element name if `tag` opens a `<script>` or `<style>` element.
///
/// `tag` starts at a `<`. The name is matched without regard to case and
/// must end at a tag-name boundary, so `<scripted>` is not a script.
fn raw_text_element_name(tag: &str) -> Option<&'static str> {
    RAW_TEXT_ELEMENTS
        .into_iter()
        .find(|name| names_element(&tag[1..], name))
}

/// Reports whether `after_bracket`, the text after `<` or `</`, starts with
/// the tag name `name` followed by whitespace, `/`, `>` or the end.
fn names_element(after_bracket: &str, name: &str) -> bool {
    let bytes = after_bracket.as_bytes();
    bytes
        .get(..name.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(name.as_bytes()))
        && bytes
            .get(name.len())
            .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
}

/// Returns what follows the `</name>` end tag in `content`, if there is one.
///
/// An end tag whose `>` is missing swallows the rest of the text.
fn skip_past_end_tag<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(found) = content[from..].find("</") {
        let after_bracket = from + found + "</".len();
        if names_element(&content[after_bracket..], name) {
            let after_name = &content[after_bracket + name.len()..];
            return Some(
                after_name
                    .find('>')
                    .map_or("", |end| &after_name[end + 1..]),
            );
        }
        from = after_bracket;
    }
    None
}

/// Longest character reference name in the HTML standard, without the `&`.
///
/// `CounterClockwiseContourIntegral;` is 32 bytes; a name scan stops there so
/// a stray `&` in long text costs a bounded lookup.
const MAX_ENTITY_NAME_LEN: usize = 32;

/// Replaces HTML character references with the characters they name.
///
/// Handles decimal (`&#233;`) and hexadecimal (`&#x20AC;`) references and
/// every named reference in the HTML standard (`&eacute;`, `&nbsp;`). An `&`
/// that does not start a reference is kept as written, so `AT&T` is unchanged.
fn decode_html_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let candidate = &rest[pos + 1..];
        match decode_entity(candidate) {
            Some((decoded, consumed)) => {
                out.push_str(&decoded);
                rest = &candidate[consumed..];
            }
            None => {
                out.push('&');
                rest = candidate;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decodes the reference at the start of `candidate`, the text after an `&`.
///
/// Returns the replacement and how many bytes of `candidate` it consumed.
fn decode_entity(candidate: &str) -> Option<(String, usize)> {
    if let Some(numeric) = candidate.strip_prefix('#') {
        let (digits, radix) = match numeric.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (numeric, 10),
        };
        let len = digits
            .bytes()
            .take_while(|byte| char::from(*byte).is_digit(radix))
            .count();
        if len == 0 {
            return None;
        }
        let code = u32::from_str_radix(&digits[..len], radix).unwrap_or(u32::MAX);
        let mut consumed = candidate.len() - digits.len() + len;
        if candidate[consumed..].starts_with(';') {
            consumed += 1;
        }
        return Some((numeric_reference_char(code).to_string(), consumed));
    }

    // Named references: extend the name while the table still knows a longer
    // one, and keep the longest complete match (`&notin;` over `&not`).
    let mut best = None;
    for (index, ch) in candidate.char_indices().take(MAX_ENTITY_NAME_LEN) {
        if !ch.is_ascii_alphanumeric() && ch != ';' {
            break;
        }
        let end = index + ch.len_utf8();
        match web_atoms::NAMED_ENTITIES.get(&candidate[..end]) {
            // The table maps every proper prefix of a name to (0, 0).
            Some(&(0, _)) => {}
            Some(&(first, second)) => {
                let mut decoded = String::new();
                decoded.extend(char::from_u32(first));
                if second != 0 {
                    decoded.extend(char::from_u32(second));
                }
                best = Some((decoded, end));
            }
            None => break,
        }
        if ch == ';' {
            break;
        }
    }
    best
}

/// Maps a numeric character reference to its character, per the HTML standard.
///
/// Code points 0x80..=0x9F are read as windows-1252 (`&#150;` is an en dash,
/// as every browser renders it); zero, surrogates and out-of-range values
/// become U+FFFD.
fn numeric_reference_char(code: u32) -> char {
    if let Ok(byte @ 0x80..=0x9F) = u8::try_from(code) {
        let bytes = [byte];
        let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&bytes);
        if let Some(ch) = decoded.chars().next() {
            return ch;
        }
    }
    match char::from_u32(code) {
        Some('\0') | None => char::REPLACEMENT_CHARACTER,
        Some(ch) => ch,
    }
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

    #[test]
    fn mbox_8bit_body_decodes_through_its_declared_charset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("latin1.mbox");
        fs::write(
            &path,
            b"From a@b Thu Jan  1 00:00:00 2024\r\n\
              Subject: =?ISO-8859-1?Q?Ol=E1?=\r\n\
              Content-Type: text/plain; charset=ISO-8859-1\r\n\
              Content-Transfer-Encoding: 8bit\r\n\
              \r\n\
              Ol\xE1 mundo\r\n\
              From c@d Thu Jan  1 00:00:01 2024\n\
              Subject: second\n\
              Content-Type: text/plain; charset=windows-1252\n\
              \n\
              \x93quoted\x94 \x80 5\n",
        )
        .unwrap();

        let mut emails = Vec::new();
        let count = parse_mbox(&path, ParseOptions::default(), |email| {
            emails.push(email);
            Ok(())
        })
        .unwrap();

        assert_eq!(count, 2);
        assert_eq!(emails[0].subject.as_deref(), Some("Olá"));
        assert_eq!(
            emails[0].body.text.as_deref().map(str::trim),
            Some("Olá mundo")
        );
        assert_eq!(
            emails[1].body.text.as_deref().map(str::trim),
            Some("“quoted” € 5")
        );
    }

    #[test]
    fn address_lists_keep_display_names_that_contain_commas() {
        let email = parse_eml_bytes(
            concat!(
                "From: =?ISO-8859-1?Q?Jos=E9_Mu=F1oz?= <jose@example.com>\r\n",
                "To: =?UTF-8?B?U8O4cmVuLCBLaWVya2VnYWFyZA==?= <s@example.com>,\r\n",
                " \"Doe, Jane\" <j@example.com>, plain@example.com\r\n",
                "Cc: Team: a@example.com, Bob <b@example.com>;, c@example.com\r\n",
                "Subject: x\r\n",
                "\r\n",
                "body\r\n",
            )
            .as_bytes(),
            ParseOptions::default(),
        )
        .unwrap();

        assert_eq!(email.from.name.as_deref(), Some("José Muñoz"));
        assert_eq!(email.from.address, "jose@example.com");

        let to: Vec<_> = email
            .to
            .iter()
            .map(|a| (a.name.as_deref(), a.address.as_str()))
            .collect();
        assert_eq!(
            to,
            [
                (Some("Søren, Kierkegaard"), "s@example.com"),
                (Some("Doe, Jane"), "j@example.com"),
                (None, "plain@example.com"),
            ]
        );

        let cc: Vec<_> = email
            .cc
            .iter()
            .map(|a| (a.name.as_deref(), a.address.as_str()))
            .collect();
        assert_eq!(
            cc,
            [
                (None, "a@example.com"),
                (Some("Bob"), "b@example.com"),
                (None, "c@example.com"),
            ]
        );
    }

    #[test]
    fn unparseable_address_headers_still_yield_what_was_written() {
        let email = parse_eml_bytes(
            b"From: not an address\r\nTo: also not one\r\nSubject: x\r\n\r\nbody\r\n",
            ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(email.from.address, "not an address");
        assert_eq!(email.to.len(), 1);
        assert_eq!(email.to[0].address, "also not one");
    }

    #[test]
    fn unknown_charset_labels_do_not_degrade_to_replacement_characters() {
        let email = parse_eml_bytes(
            b"Subject: =?x-unknown?Q?Ol=E1?=\r\n\
              Content-Type: text/plain; charset=x-unknown\r\n\
              Content-Transfer-Encoding: 8bit\r\n\
              \r\n\
              Ol\xE1 mundo\r\n",
            ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(email.subject.as_deref(), Some("Olá"));
        assert_eq!(email.body.text.as_deref().map(str::trim), Some("Olá mundo"));

        // The same label over valid UTF-8 bytes is read as UTF-8.
        let email = parse_eml_bytes(
            "Subject: x\r\nContent-Type: text/plain; charset=x-unknown\r\n\r\nOlá € 日本\r\n"
                .as_bytes(),
            ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            email.body.text.as_deref().map(str::trim),
            Some("Olá € 日本")
        );
    }

    #[test]
    fn declared_charsets_still_decode_as_declared() {
        let email = parse_eml_bytes(
            b"Subject: =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=\r\n\
              Content-Type: text/plain; charset=Shift_JIS\r\n\
              Content-Transfer-Encoding: base64\r\n\
              \r\n\
              k/qWe4zq\r\n",
            ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(email.subject.as_deref(), Some("日本語"));
        assert_eq!(email.body.text.as_deref().map(str::trim), Some("日本語"));
    }

    #[test]
    fn html_to_text_decodes_character_references_after_stripping_tags() {
        assert_eq!(
            html_to_text("<p>Caf&eacute;&nbsp;&amp; &#233; &#x20AC; &rsquo;</p>"),
            "\nCafé\u{a0}& é € ’\n"
        );
        // An escaped tag is text, not markup, and must survive.
        assert_eq!(html_to_text("<b>a &lt;b&gt; c</b>"), "a <b> c");
        // `&` that starts no reference is kept as written.
        assert_eq!(
            html_to_text("AT&T & Sons &; &#; &#x; &unknown;"),
            "AT&T & Sons &; &#; &#x; &unknown;"
        );
        // Longest name wins, legacy names need no semicolon, and 0x80..=0x9F
        // numeric references follow windows-1252.
        assert_eq!(
            html_to_text("&notin; &not; &copy 2024 &#150; &#x99;"),
            "∉ ¬ © 2024 – ™"
        );
        // Two-code-point references and invalid numbers.
        assert_eq!(
            html_to_text("&NotEqualTilde; &#0; &#xD800; &#x110000;"),
            "≂̸ \u{fffd} \u{fffd} \u{fffd}"
        );
        assert_eq!(html_to_text("ol&aacute;"), "olá");
    }
}

/// Regressions for malformed and adversarial input. Each test here failed
/// before the fix it covers.
#[cfg(test)]
mod malformed_input_regression_tests {
    use super::*;
    use tempfile::tempdir;

    /// Parses `mailbox` and returns each message's subject and text body.
    fn mbox_messages(mailbox: &[u8]) -> Vec<(String, String)> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mailbox.mbox");
        std::fs::write(&path, mailbox).unwrap();
        let mut messages = Vec::new();
        parse_mbox(&path, ParseOptions::default(), |email| {
            messages.push((
                email.subject.unwrap_or_default(),
                email.body.text.unwrap_or_default(),
            ));
            Ok(())
        })
        .unwrap();
        messages
    }

    #[test]
    fn address_with_the_closing_bracket_first_is_kept_as_written() {
        let addr = parse_email_address("a> <b").unwrap();
        assert_eq!(addr.name, None);
        assert_eq!(addr.address, "a> <b");

        // A stray `>` before the real brackets does not hide them.
        let addr = parse_email_address("x> Name <a@example.com> tail>").unwrap();
        assert_eq!(addr.name.as_deref(), Some("x> Name"));
        assert_eq!(addr.address, "a@example.com");
    }

    #[test]
    fn mbox_body_lines_starting_with_from_do_not_split_the_message() {
        let messages = mbox_messages(
            concat!(
                "From a@example.com Mon Jan 15 14:30:00 2024\n",
                "Subject: one\n",
                "\n",
                "From now on, use the new address.\n",
                "From Monday to Friday, 9:00 to 17:00\n",
                "From me, Mon Jan 15 at noon\n",
                "\n",
                "From b@example.com Tue Jan 16 08:00:00 2024\n",
                "Subject: two\n",
                "\n",
                "body two\n",
            )
            .as_bytes(),
        );
        let subjects: Vec<&str> = messages.iter().map(|(subject, _)| &**subject).collect();
        assert_eq!(subjects, ["one", "two"]);
        assert!(
            messages[0].1.contains("From now on, use the new address.")
                && messages[0].1.contains("From Monday to Friday")
                && messages[0].1.contains("From me, Mon Jan 15 at noon"),
            "{:?}",
            messages[0].1
        );
    }

    #[test]
    fn html_to_text_drops_script_style_and_comment_content() {
        assert_eq!(
            html_to_text("<style>p { color: red }</style><p>Hi</p><script>alert('x < y')</script>"),
            "\nHi\n"
        );
        assert_eq!(html_to_text("a<!-- hidden <b> -->b"), "ab");
        // Names match without regard to case, attributes are allowed, and a
        // `</b>` inside the script is script text, not markup.
        assert_eq!(
            html_to_text("a<SCRIPT type=\"text/javascript\">var x = '</b>';</ScRiPt >b"),
            "ab"
        );
        assert_eq!(html_to_text("a<style\nmedia=print>p{}</style\n>b"), "ab");
        // Unterminated elements and comments swallow the rest, as in a browser.
        assert_eq!(html_to_text("a<style>p{} b"), "a");
        assert_eq!(html_to_text("a<script>x() b"), "a");
        assert_eq!(html_to_text("a<script x() b"), "a");
        assert_eq!(html_to_text("a<!-- never closed b"), "a");
    }
}

/// Tests of the helpers the malformed-input fixes introduced, and of valid
/// input those fixes must keep accepting.
#[cfg(test)]
mod malformed_input_unit_tests {
    use super::*;

    #[test]
    fn html_to_text_keeps_text_that_only_resembles_markup() {
        // Escaped markup is text, whatever element it names.
        assert_eq!(
            html_to_text("&lt;script&gt;x&lt;/script&gt; &lt;!-- c --&gt;"),
            "<script>x</script> <!-- c -->"
        );
        // `<scripted>` and `<styles>` are ordinary tags.
        assert_eq!(html_to_text("<scripted>x</scripted>"), "x");
        assert_eq!(html_to_text("<styles>y</styles>"), "y");
        // The two empty comment forms end where they start.
        assert_eq!(html_to_text("<!-->a<!--->b"), "ab");
        // A `<` that nothing closes, and `<>`, are text.
        assert_eq!(html_to_text("a << b"), "a << b");
        assert_eq!(html_to_text("x <> y < z"), "x <> y < z");
        assert_eq!(html_to_text("<b>bold</b> < <i>i</i>"), "bold < i");
        assert_eq!(
            html_to_text("<P>one<br>two<br/>three</p>"),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn mbox_separator_accepts_the_date_forms_mail_software_writes() {
        for line in [
            "From a@example.com Mon Jan 15 14:30:00 2024",
            "From a@example.com Mon Jan  1 00:00:00 2024",
            "From a@b Thu Jan  1 00:00:00 2024",
            "From MAILER-DAEMON Mon Jan 15 14:30 2024",
            "From - Mon Jan 15 14:30:00 2024",
            "From 1719000000000000000@xxx Mon Jan 15 14:30:00 +0000 2024",
            "From a@example.com Mon Jan 15 14:30:00 2024 +0000",
            "From a@example.com Mon Jan 15 14:30:00 EST 2024",
            "From \"a b\"@example.com Mon Jan 15 14:30:00 2024",
            "From  Mon Jan 15 14:30:00 2024",
            "From a@example.com mon jan 15 14:30:00 2024 remote from host",
        ] {
            assert!(is_mbox_separator(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn mbox_separator_rejects_body_text() {
        for line in [
            "From now on, use the new address.",
            "From Monday to Friday, 9:00 to 17:00",
            "From me, Mon Jan 15 at noon",
            "From a@example.com Mon Jan 15 14:30:00",
            "From a@example.com Mon Jan 15 2024",
            "From a@example.com Mon Jan 150 14:30:00 2024",
            "From a@example.com Mon Jan 15 14:30:00:00:00 2024",
            "From a@example.com Mon Jan 15 14:30:00 a b c 2024",
            "From: a@example.com",
            "From ",
            ">From a@example.com Mon Jan 15 14:30:00 2024",
        ] {
            assert!(!is_mbox_separator(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn mbox_first_separator_needs_no_date() {
        // Some tools write a bare first line. With nothing before it there is
        // no message for it to split, so it is accepted as written.
        let mut subjects = Vec::new();
        let count = parse_mbox_reader(
            &b"From sender\nSubject: one\n\nFrom then on.\n"[..],
            ParseOptions::default(),
            |email| {
                subjects.push(email.subject.unwrap_or_default());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((count, subjects), (1, vec!["one".to_string()]));
    }

    /// Depth of the tree `mailparse` built, measured without recursion.
    fn parsed_depth(mail: &mailparse::ParsedMail<'_>) -> usize {
        let mut deepest = 0;
        let mut pending = vec![(mail, 0)];
        while let Some((part, depth)) = pending.pop() {
            deepest = deepest.max(depth);
            pending.extend(part.subparts.iter().map(|sub| (sub, depth + 1)));
        }
        deepest
    }

    /// Writes one MIME part: a leaf, or a container of up to three parts.
    ///
    /// Boundaries are chosen to collide on purpose (one is a prefix of the
    /// others, some repeat), headers vary in case and quoting, and closing
    /// delimiters are sometimes left out, so the generated trees exercise the
    /// corners of the splitting rules rather than only well-formed mail.
    fn write_part(rng: &mut smoke::Rng, out: &mut Vec<u8>, levels: usize, next_id: &mut usize) {
        if levels == 0 || rng.below(4) == 0 {
            out.extend_from_slice(b"Content-Type: text/plain\r\n\r\nleaf\r\n-- \r\nsignature\r\n");
            return;
        }
        *next_id += 1;
        let boundary = match rng.below(4) {
            0 => "b".to_string(),
            1 => format!("b{}", *next_id % 3),
            _ => format!("part{next_id:03}"),
        };
        let header = match rng.below(4) {
            0 => format!("content-type: MULTIPART/Mixed; BOUNDARY=\"{boundary}\""),
            1 => format!("Content-Type: multipart/alternative;\r\n boundary={boundary}"),
            2 => format!("Content-Type: multipart/digest; boundary=\"{boundary}\"; x=y"),
            _ => format!("Content-Type: multipart/mixed; boundary={boundary}"),
        };
        let newline: &[u8] = if rng.below(4) == 0 { b"\n" } else { b"\r\n" };
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(newline);
        out.extend_from_slice(newline);
        if rng.below(3) == 0 {
            out.extend_from_slice(b"preamble\r\n");
        }
        for _ in 0..1 + rng.below(3) {
            out.extend_from_slice(format!("--{boundary}").as_bytes());
            out.extend_from_slice(newline);
            write_part(rng, out, levels - 1, next_id);
        }
        match rng.below(4) {
            0 => {}
            1 => out.extend_from_slice(format!("--{boundary}--").as_bytes()),
            _ => out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes()),
        }
    }

    #[test]
    fn mime_depth_walk_agrees_with_mailparse() {
        let mut rng = smoke::Rng::new(0x00C0_FFEE);
        let mut compared = 0;
        let mut deepest_seen = 0;
        for _ in 0..400 {
            let mut message = b"From: a@example.com\r\nSubject: tree\r\n".to_vec();
            let levels = 1 + rng.below(9);
            write_part(&mut rng, &mut message, levels, &mut 0);
            let mut inputs = vec![message.clone()];
            inputs.extend((0..6).map(|_| smoke::mutate(&message, &mut rng)));
            for input in inputs {
                let Ok(mail) = mailparse::parse_mail(&input) else {
                    // The walk must still terminate on what `mailparse` rejects.
                    let _ = mime_nesting_depth(&input, usize::MAX);
                    continue;
                };
                let depth = parsed_depth(&mail);
                assert_eq!(
                    mime_nesting_depth(&input, usize::MAX),
                    Some(depth),
                    "walk disagrees with mailparse on {:?}",
                    String::from_utf8_lossy(&input)
                );
                for limit in 0..=depth + 1 {
                    assert_eq!(
                        mime_nesting_exceeds(&input, limit),
                        depth > limit,
                        "limit {limit}, depth {depth}: {:?}",
                        String::from_utf8_lossy(&input)
                    );
                }
                compared += 1;
                deepest_seen = deepest_seen.max(depth);
            }
        }
        // Guard against a generator that stops producing nested, parseable mail.
        assert!(compared > 1_500, "only {compared} inputs compared");
        assert!(deepest_seen >= 6, "deepest tree was {deepest_seen}");
    }

    #[test]
    fn many_dash_lines_in_a_shallow_message_are_not_nesting() {
        // Quoted replies, signatures and diffs are full of lines starting
        // with `--`. They cost a walk of the tree, not a rejection.
        let mut message = concat!(
            "From: a@example.com\r\n",
            "Subject: thread\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n",
            "\r\n",
            "--outer\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
        )
        .as_bytes()
        .to_vec();
        for _ in 0..500 {
            message.extend_from_slice(b"-----Original Message-----\r\n-- \r\n--- a/file\r\n");
        }
        message.extend_from_slice(b"--outer--\r\n");
        assert_eq!(mime_nesting_depth(&message, MAX_MIME_DEPTH), Some(1));
        let email = parse_eml_bytes(&message, ParseOptions::default()).expect("shallow mail");
        assert!(email
            .body
            .text
            .unwrap_or_default()
            .contains("-----Original Message-----"));
    }

    #[test]
    fn nesting_at_the_limit_parses_and_one_past_it_does_not() {
        let nested = |depth: usize| {
            let mut out = b"Subject: deep\r\n".to_vec();
            for level in 0..depth {
                out.extend_from_slice(
                    format!(
                        "Content-Type: multipart/mixed; boundary=b{level:04}\r\n\r\n--b{level:04}\r\n"
                    )
                    .as_bytes(),
                );
            }
            out.extend_from_slice(b"Content-Type: text/plain\r\n\r\nleaf\r\n");
            out
        };
        let at_limit = parse_eml_bytes(&nested(MAX_MIME_DEPTH), ParseOptions::default())
            .expect("nesting at the limit");
        assert_eq!(at_limit.body.text.as_deref().map(str::trim), Some("leaf"));
        let error = parse_eml_bytes(&nested(MAX_MIME_DEPTH + 1), ParseOptions::default())
            .expect_err("nesting past the limit");
        assert!(error.to_string().contains("nesting"), "{error}");
    }
}
