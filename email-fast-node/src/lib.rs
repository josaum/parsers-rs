// Rust guideline compliant 2026-02-21
//! Node-API bindings for `email-fast-core`.
//!
//! Every function takes a file path, mirrors the Python binding one-for-one,
//! and reports parse failures as JavaScript errors. Options objects are
//! partial: an omitted field takes the core crate's default.

use email_fast_core::arrow::{parse_mbox_to_arrow_ipc, ArrowOptions};
use email_fast_core::{
    extract_attachments, parse_email, parse_email_to_ndjson, parse_eml, parse_mbox_to_ndjson,
    ParseOptions,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Parse options exposed to Node.js consumers.
///
/// All fields are optional; omitted fields fall back to [`ParseOptions::default()`].
#[napi(object)]
pub struct ParseOptionsJs {
    /// Maximum size of one attachment, in bytes (default: 100 MB).
    pub max_attachment_size: Option<u32>,
    /// Include raw headers in the output (default: false).
    pub include_raw_headers: Option<bool>,
    /// Convert the HTML body to plain text (default: false).
    pub html_to_text: Option<bool>,
    /// Extract attachment content as Base64 (default: false).
    pub extract_attachment_content: Option<bool>,
}

/// Arrow IPC output options exposed to Node.js consumers.
///
/// Kept separate from [`ParseOptionsJs`] for consistency with the docx and pptx
/// Node.js bindings.
#[napi(object)]
pub struct ArrowOptionsJs {
    /// Emails per Arrow record batch (default: the core crate's).
    pub batch_size: Option<u32>,
}

/// Build [`ParseOptions`] from a partial JS object, filling unset fields from the core default.
///
/// The aggregate attachment budget (`max_total_attachment_bytes`) is not
/// exposed: it stays at the core default so a caller raising the per-part
/// ceiling cannot lift the sum without a deliberate binding change.
fn to_parse_options(opts: Option<ParseOptionsJs>) -> ParseOptions {
    let d = ParseOptions::default();
    match opts {
        None => d,
        Some(o) => ParseOptions {
            max_attachment_size: o
                .max_attachment_size
                .map_or(d.max_attachment_size, |v| v as usize),
            include_raw_headers: o.include_raw_headers.unwrap_or(d.include_raw_headers),
            html_to_text: o.html_to_text.unwrap_or(d.html_to_text),
            extract_attachment_content: o
                .extract_attachment_content
                .unwrap_or(d.extract_attachment_content),
            ..d
        },
    }
}

/// Build [`ArrowOptions`] from a partial JS object, filling unset fields from the core default.
fn to_arrow_options(opts: Option<ArrowOptionsJs>) -> ArrowOptions {
    let d = ArrowOptions::default();
    ArrowOptions {
        batch_size: opts
            .and_then(|o| o.batch_size)
            .map_or(d.batch_size, |v| v as usize),
    }
}

/// Convert a core error into a JavaScript error carrying its message.
fn js_error(err: impl std::fmt::Display) -> Error {
    Error::from_reason(err.to_string())
}

/// Parse an email file (EML or MSG, detected by content) and return it as a JSON value.
#[napi(catch_unwind)]
pub fn parse_email_js(path: String, options: Option<ParseOptionsJs>) -> Result<serde_json::Value> {
    let email = parse_email(path, to_parse_options(options)).map_err(js_error)?;
    serde_json::to_value(&email).map_err(js_error)
}

/// Parse an RFC 5322 EML file and return it as a JSON value.
#[napi(catch_unwind)]
pub fn parse_eml_js(path: String, options: Option<ParseOptionsJs>) -> Result<serde_json::Value> {
    let email = parse_eml(path, to_parse_options(options)).map_err(js_error)?;
    serde_json::to_value(&email).map_err(js_error)
}

/// Parse an email file and write each message as one NDJSON record to `out_path`.
#[napi(catch_unwind)]
pub fn parse_email_ndjson_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
) -> Result<()> {
    parse_email_to_ndjson(path, out_path, to_parse_options(options)).map_err(js_error)
}

/// Parse an mbox file into NDJSON at `out_path` and return the message count.
#[napi(catch_unwind)]
pub fn parse_mbox_ndjson_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
) -> Result<u32> {
    let count =
        parse_mbox_to_ndjson(path, out_path, to_parse_options(options)).map_err(js_error)?;
    u32::try_from(count).map_err(js_error)
}

/// Parse an mbox file into an Arrow IPC file at `out_path` and return the message count.
#[napi(catch_unwind)]
pub fn parse_mbox_arrow_js(
    path: String,
    out_path: String,
    options: Option<ParseOptionsJs>,
    arrow_options: Option<ArrowOptionsJs>,
) -> Result<u32> {
    let count = parse_mbox_to_arrow_ipc(
        path,
        out_path,
        to_parse_options(options),
        to_arrow_options(arrow_options),
    )
    .map_err(js_error)?;
    u32::try_from(count).map_err(js_error)
}

/// Extract every attachment of an email file into `out_dir` and return the written paths.
///
/// `max_attachment_size` overrides the per-attachment ceiling; content
/// extraction is always on because the bytes are what gets written.
#[napi(catch_unwind)]
pub fn extract_attachments_js(
    path: String,
    out_dir: String,
    max_attachment_size: Option<u32>,
) -> Result<Vec<String>> {
    let d = ParseOptions::default();
    let options = ParseOptions {
        max_attachment_size: max_attachment_size.map_or(d.max_attachment_size, |v| v as usize),
        extract_attachment_content: true,
        ..d
    };
    let paths = extract_attachments(path, out_dir, options).map_err(js_error)?;
    Ok(paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect())
}
