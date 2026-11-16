//! What every email fuzz target does with its input, shared by path.

use email_fast_core::{Email, ParseOptions};

/// The default options, then every optional code path switched on.
pub fn option_sets() -> [ParseOptions; 2] {
    [
        ParseOptions::default(),
        ParseOptions {
            html_to_text: true,
            include_raw_headers: true,
            extract_attachment_content: true,
            ..ParseOptions::default()
        },
    ]
}

/// Touches the accessors a consumer calls on a parsed message.
pub fn consume(email: &Email) {
    let _ = email.has_substantive_content();
    for attachment in &email.attachments {
        let _ = attachment.extension();
        let _ = attachment.is_nested_message();
        let _ = attachment.decode_content();
    }
    let _ = serde_json::to_string(email);
}
