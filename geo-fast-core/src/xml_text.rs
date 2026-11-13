//! Character data of an XML element, put back together from its pieces.
//!
//! quick-xml reports an entity or character reference as an event of its own,
//! so `a &amp; b` arrives as the text `a `, the reference `amp` and the text
//! ` b`. A reader that assigns a field per text event would keep ` b`.
//! [`TextRun`] collects the pieces between two pieces of markup and hands
//! back the one string they spell.

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesCData, BytesRef, BytesText};

/// Whitespace as XML defines it: space, tab, carriage return and line feed.
///
/// Narrower than [`char::is_whitespace`]. Only these four are insignificant
/// around character data; a no-break space is content.
fn is_xml_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// The character data between two pieces of markup.
///
/// Feed it every text, reference and CDATA event, and call [`TextRun::take`]
/// when any other event arrives. A CDATA section is text like the rest: its
/// content is taken as written, markup characters and whitespace included. Literal whitespace at either end of the run is
/// dropped; whitespace written as a reference (`&#32;`) is content and stays.
///
/// A reference that cannot be resolved (an entity that is not one of XML's
/// five predefined ones, or a character reference to no character) is kept as
/// it was written, ampersand and semicolon included.
#[derive(Debug, Default)]
pub(crate) struct TextRun {
    text: String,
    /// Bytes at the end of `text` that are literal whitespace.
    trailing_whitespace: usize,
    /// Whether anything other than leading literal whitespace has arrived.
    started: bool,
    /// Whether a piece was not valid in the document's encoding.
    undecodable: bool,
}

impl TextRun {
    /// Appends a piece of literal text.
    pub(crate) fn push_text(&mut self, piece: &BytesText<'_>) {
        let Ok(decoded) = piece.decode() else {
            self.started = true;
            self.undecodable = true;
            return;
        };
        let mut piece: &str = &decoded;
        if !self.started {
            piece = piece.trim_start_matches(is_xml_whitespace);
            if piece.is_empty() {
                return;
            }
            self.started = true;
        }
        let content_len = piece.trim_end_matches(is_xml_whitespace).len();
        if content_len == 0 {
            self.trailing_whitespace += piece.len();
        } else {
            self.trailing_whitespace = piece.len() - content_len;
        }
        self.text.push_str(piece);
    }

    /// Appends the content of a CDATA section, exactly as written.
    ///
    /// An empty section adds nothing.
    pub(crate) fn push_cdata(&mut self, section: &BytesCData<'_>) {
        let Ok(content) = section.decode() else {
            self.started = true;
            self.undecodable = true;
            return;
        };
        if content.is_empty() {
            return;
        }
        self.started = true;
        self.trailing_whitespace = 0;
        self.text.push_str(&content);
    }

    /// Appends what an entity or character reference stands for.
    pub(crate) fn push_reference(&mut self, reference: &BytesRef<'_>) {
        self.started = true;
        let Ok(name) = reference.decode() else {
            self.undecodable = true;
            return;
        };
        self.trailing_whitespace = 0;
        if let Ok(Some(character)) = reference.resolve_char_ref() {
            self.text.push(character);
        } else if let Some(replacement) = resolve_predefined_entity(&name) {
            self.text.push_str(replacement);
        } else {
            self.text.push('&');
            self.text.push_str(&name);
            self.text.push(';');
        }
    }

    /// Returns the collected text and starts a new run.
    ///
    /// `None` means the run held nothing but literal whitespace. A run with a
    /// piece that could not be decoded yields an empty string: its value is
    /// unknown, but the element did have one.
    pub(crate) fn take(&mut self) -> Option<String> {
        let run = std::mem::take(self);
        if !run.started {
            return None;
        }
        if run.undecodable {
            return Some(String::new());
        }
        let mut text = run.text;
        text.truncate(text.len().saturating_sub(run.trailing_whitespace));
        Some(text)
    }
}
