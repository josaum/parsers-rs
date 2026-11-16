//! Strict byte decoding with bounded, markup-aware meta discovery.
use crate::WebParseError;
use encoding_rs::{Encoding, UTF_8};
use scraper::{ElementRef, Html};

pub(crate) fn resolve_encoding(
    bytes: &[u8],
    explicit_label: Option<&str>,
) -> Result<(&'static Encoding, usize), WebParseError> {
    let bom = Encoding::for_bom(bytes);
    if let Some(label) = explicit_label {
        let encoding =
            Encoding::for_label(label.as_bytes()).ok_or(WebParseError::InvalidEncoding)?;
        if bom.is_some_and(|(detected, _)| detected != encoding) {
            return Err(WebParseError::InvalidEncoding);
        }
        return Ok((encoding, bom.map_or(0, |(_, len)| len)));
    }
    if let Some(bom) = bom {
        return Ok(bom);
    }
    Ok((meta_encoding(bytes).unwrap_or(UTF_8), 0))
}

fn meta_encoding(bytes: &[u8]) -> Option<&'static Encoding> {
    let prefix = &bytes[..bytes.len().min(1024)];
    if !prefix
        .windows(5)
        .any(|part| part.eq_ignore_ascii_case(b"<meta"))
    {
        return None;
    }
    // HTML parsing prevents comments, quoted attributes, and raw-text elements
    // from impersonating declarations. Only the bounded prefix is inspected.
    let html = Html::parse_document(&String::from_utf8_lossy(prefix));
    for element in html.tree.root().descendants().filter_map(ElementRef::wrap) {
        if element.value().name() != "meta"
            || element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|parent| parent.value().name() == "template")
        {
            continue;
        }
        let encoding = if let Some(label) = element.value().attr("charset") {
            Encoding::for_label(label.as_bytes())
        } else if element
            .value()
            .attr("http-equiv")
            .is_some_and(|value| value.eq_ignore_ascii_case("content-type"))
        {
            element.value().attr("content").and_then(content_encoding)
        } else {
            None
        };
        if let Some(encoding) = encoding {
            return Some(
                if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
                    UTF_8
                } else if encoding == encoding_rs::X_USER_DEFINED {
                    encoding_rs::WINDOWS_1252
                } else {
                    encoding
                },
            );
        }
    }
    None
}

fn content_encoding(content: &str) -> Option<&'static Encoding> {
    let bytes = content.as_bytes();
    for (index, part) in bytes.windows(7).enumerate() {
        if !part.eq_ignore_ascii_case(b"charset") {
            continue;
        }
        let mut rest = &content[index + 7..];
        rest = rest.trim_start_matches(ascii_space);
        let Some(value) = rest.strip_prefix('=') else {
            continue;
        };
        rest = value.trim_start_matches(ascii_space);
        let label = if rest.starts_with(['\'', '"']) {
            let quote = rest.as_bytes()[0] as char;
            let value = &rest[1..];
            &value[..value.find(quote)?]
        } else {
            &rest[..rest
                .find(|c| ascii_space(c) || c == ';')
                .unwrap_or(rest.len())]
        };
        return Encoding::for_label(label.as_bytes());
    }
    None
}

pub(crate) fn ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}
