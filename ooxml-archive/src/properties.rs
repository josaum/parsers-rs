// Rust guideline compliant 2026-02-21
//! Core and extended properties (`docProps/core.xml`, `docProps/app.xml`).
//!
//! The Word and PowerPoint parsers read both parts through
//! [`parse_simple_props`], so a part gives the same properties in either.

use anyhow::{anyhow, Result};
use chrono::DateTime;
use quick_xml::events::{BytesRef, Event};
use std::collections::HashMap;
use std::io::Cursor;

use crate::{local_name, xml_reader};

/// Reads `docProps/core.xml` or `docProps/app.xml`.
///
/// Every child of the root that holds text is a property, named by its
/// local name (`Company`, `title`). The root element itself is not one.
///
/// A property whose value is nested, as the vectors of `docProps/app.xml`
/// are (`TitlesOfParts`, `HeadingPairs`), keeps the name of the property:
/// the elements inside it (`vt:vector`, `vt:variant`, `vt:lpstr`, `vt:i4`)
/// say how the value is typed and are not properties. Its value is the text
/// of every item, in document order, one per line. So `TitlesOfParts` with
/// two `vt:lpstr` items reads `"First\nSecond"`.
///
/// An item is an element inside the property that holds no other element
/// and is not one of the containers `vt:vector`, `vt:array` and
/// `vt:variant`. Every item is one line, an empty one included, so the
/// number of lines is the number of items and the pairs of `HeadingPairs`
/// can be matched with `TitlesOfParts` again: three items of which only the
/// second has text read `"\nScope\n"`. A property with one empty item reads
/// `""`; a property with no item is not stored. A line break inside an item
/// (`\r\n`, `\n` or `\r`) reads as one space, so that a line is never part
/// of an item; the break itself is not recoverable.
///
/// A property that is not nested is not touched by any of this: its text is
/// stored as it is, line breaks included, and not at all when it is empty.
/// Whitespace that is the layout of the file is not a value: the
/// indentation between two elements, and an element holding nothing but
/// whitespace with a line break in it. Spaces alone are a value.
///
/// A value that is an RFC 3339 date and time is written in one form
/// (`2024-01-02T03:04:05Z` reads `2024-01-02T03:04:05+00:00`).
///
/// `xml` must be UTF-8; a caller that accepts UTF-16 parts transcodes them
/// first. A stray byte that is not UTF-8 reads as U+FFFD.
///
/// # Errors
///
/// Returns an error when `xml` is not well-formed enough to read, for
/// example when an end tag does not match its start tag.
pub fn parse_simple_props(xml: &[u8]) -> Result<HashMap<String, String>> {
    let xml = crate::utf8::lossy_utf8(xml);
    let mut reader = xml_reader(Cursor::new(xml.as_ref()));
    let mut buf = Vec::new();
    let mut out: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    // The child of the root being read, and whether the text now collected
    // stands in an element nested inside it.
    let mut property: Option<String> = None;
    let mut nested = false;
    // Whether the element now open is a nested item: inside a property, not
    // a container, and with no element met inside it so far.
    let mut item_open = false;
    let mut value = String::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                if current_key.is_some() {
                    value.push_str(&character_data(&event).unwrap_or_default());
                }
            }
            Ok(event) => {
                if item_open && matches!(event, Event::End(_)) {
                    // The end of an item: its text is a line whatever it is.
                    if let Some(key) = current_key.take() {
                        push_nested_item(&mut out, key, &nested_item_line(&value));
                    }
                    value.clear();
                }
                let before_element = matches!(event, Event::Start(_) | Event::Empty(_));
                let mut stored = HashMap::new();
                store_property(&mut stored, &mut current_key, &mut value, before_element);
                for (key, text) in stored {
                    if nested {
                        // Text beside the items, inside a container.
                        push_nested_item(&mut out, key, &single_line(&text));
                    } else {
                        out.insert(key, text);
                    }
                }
                match event {
                    Event::Start(e) => {
                        let raw = e.name().as_ref().as_bytes().to_vec();
                        let name = local_name(&raw);
                        if depth == 1 {
                            property = Some(String::from_utf8_lossy(name).to_string());
                        }
                        if depth > 0 {
                            current_key.clone_from(&property);
                            nested = depth > 1;
                        }
                        item_open = depth > 1 && !is_property_container(name);
                        depth += 1;
                    }
                    Event::Empty(e) => {
                        // A self-closing element inside a property is an
                        // item with no text.
                        let raw = e.name().as_ref().as_bytes().to_vec();
                        if depth > 1 && !is_property_container(local_name(&raw)) {
                            if let Some(key) = property.clone() {
                                push_nested_item(&mut out, key, "");
                            }
                        }
                        item_open = false;
                    }
                    Event::End(_) => {
                        item_open = false;
                        current_key = None;
                        depth = depth.saturating_sub(1);
                        if depth <= 1 {
                            property = None;
                        }
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
            Err(err) => return Err(anyhow!("parse props: {err}")),
        }
        buf.clear();
    }
    Ok(out)
}

/// Stores the character data collected for `key`, if there is any.
///
/// A value can arrive in several pieces (text, a reference, a CDATA
/// section), so it is collected until something other than character data
/// ends it. The first run of character data after the key is the value;
/// taking the key stops later runs from replacing it.
///
/// A run of nothing but whitespace is the layout of the file, not a value,
/// in two cases: when another element opens right after it
/// (`before_element`), which is the indentation between two elements and
/// leaves the key open for the value element that follows; and when it
/// holds a line break, which is the indentation inside an element that has
/// no content. Spaces that fill an element up to its end tag are a value.
fn store_property(
    out: &mut HashMap<String, String>,
    key: &mut Option<String>,
    value: &mut String,
    before_element: bool,
) {
    let is_whitespace = value
        .chars()
        .all(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'));
    let is_layout = is_whitespace && (before_element || value.contains(['\r', '\n']));
    if !value.is_empty() && !is_layout {
        if let Some(key) = key.take() {
            out.insert(key, normalize_datetime(value));
        }
    }
    value.clear();
}

/// Whether an element inside a property groups items instead of being one.
fn is_property_container(local: &[u8]) -> bool {
    matches!(local, b"vector" | b"array" | b"variant")
}

/// Adds one item of a nested property as a line of its own.
fn push_nested_item(out: &mut HashMap<String, String>, key: String, text: &str) {
    match out.get_mut(&key) {
        Some(items) => {
            items.push('\n');
            items.push_str(text);
        }
        None => {
            out.insert(key, text.to_string());
        }
    }
}

/// The line that stands for one nested item.
///
/// Nothing but whitespace with a line break in it is the layout of an
/// element with no content, and reads as empty. Any other line break in the
/// item reads as one space: lines separate items, so an item cannot hold
/// one.
fn nested_item_line(value: &str) -> String {
    let is_layout = value.contains(['\r', '\n'])
        && value
            .chars()
            .all(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'));
    if is_layout {
        return String::new();
    }
    normalize_datetime(&single_line(value))
}

/// `value` with every line break (`\r\n`, `\n` or `\r`) read as one space.
fn single_line(value: &str) -> String {
    value.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// `value` in the one form chrono writes, when it is an RFC 3339 date and
/// time; otherwise `value` as it is.
fn normalize_datetime(value: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return dt.to_rfc3339();
    }
    value.to_string()
}

/// Returns the character data of a text, CDATA or reference event, `None`
/// for any other.
///
/// Text and CDATA are decoded leniently: a stray non-UTF-8 byte becomes
/// U+FFFD, so one bad character costs that character rather than the whole
/// part. CDATA is literal character data, never entity-unescaped. A
/// reference is resolved like [`reference_string`].
fn character_data(event: &Event<'_>) -> Option<String> {
    match event {
        Event::Text(text) => Some(text.to_string()),
        Event::CData(data) => Some(data.to_string()),
        Event::GeneralRef(reference) => Some(reference_string(reference)),
        _ => None,
    }
}

/// Resolves an entity or character reference to the text it stands for.
///
/// The five entities XML predefines and decimal or hexadecimal character
/// references resolve to their character. Anything else, an unknown entity
/// or a character reference that names no character, is kept as written
/// (`&name;`) rather than dropped.
fn reference_string(reference: &BytesRef<'_>) -> String {
    let name: &str = reference;
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => ch.to_string(),
        Ok(None) => match quick_xml::escape::resolve_xml_entity(name) {
            Some(text) => text.to_string(),
            None => format!("&{name};"),
        },
        Err(_) => format!("&{name};"),
    }
}
