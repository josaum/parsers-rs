//! XML validator that uses the parsed [`SchemaModel`] to check documents.
//!
//! Validates simple-type constraints: enumerations, string length, numeric range.
//! Complex structure checks (required children, cardinality) are also included.
//!
//! Each [`TypeFrame`] on the stack borrows its type from the schema model, so
//! the cost of an open element does not depend on the size of its type.

use std::collections::HashMap;

use anyhow::Result;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::parser::SchemaSet;
use crate::schema::{ComplexType, ContentModel, ElementRef, SchemaModel, SimpleTypeDef, XsdType};

/// Most bytes of allowed values quoted in one enumeration finding.
///
/// A finding names the values the element could have had. An enumeration can
/// be as large as the schema, and one finding is produced per invalid element,
/// so quoting it in full would copy the schema once per finding. Past this
/// many bytes the message says how many values were left out.
const MAX_ENUMERATION_LISTING_BYTES: usize = 1024;

/// Severity of a validation issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationSeverity {
    /// Schema constraint violated.
    Error,
    /// Non-fatal inconsistency (e.g. unexpected numeric text).
    Warning,
}

/// A single validation finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    /// Error or Warning.
    pub severity: ValidationSeverity,
    /// XPath-like location, e.g. `/mensagemschema/prestadorParaOperadora`.
    pub path: String,
    /// Human-readable description.
    pub message: String,
}

/// Options for the validation phase.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Most findings to report, warnings included. Must be at least 1.
    /// Default: 100.
    ///
    /// Once this many findings are recorded the document is no longer
    /// checked against the schema, so a result holding exactly this many may
    /// be incomplete. The rest of the document is still read, and a parse
    /// error raised there is reported as one more finding.
    ///
    /// Zero is refused: with no room for a finding every document would be
    /// reported as clean.
    pub max_errors: usize,
    /// Validate simple-type facet constraints (enum, length, range). Default: true.
    pub check_facets: bool,
    /// Validate required/optional cardinality. Default: true.
    pub check_cardinality: bool,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        Self {
            max_errors: 100,
            check_facets: true,
            check_cardinality: true,
        }
    }
}

impl SchemaSet {
    /// Validate an XML document against this schema set.
    ///
    /// Returns the findings, at most [`ValidateOptions::max_errors`] of them
    /// from checking the document against the schema. A parse error raised
    /// while reading the document, such as a mismatched end tag, is always
    /// among the findings, as one more than the limit if the limit was already
    /// reached. A document that merely ends with elements still open raises
    /// no parse error and is not reported. A result at the limit may be
    /// incomplete.
    ///
    /// # Errors
    ///
    /// Fails when [`ValidateOptions::max_errors`] is zero, because that limit
    /// would report every document as clean.
    pub fn validate(&self, xml: &[u8], opts: ValidateOptions) -> Result<Vec<ValidationError>> {
        anyhow::ensure!(
            opts.max_errors > 0,
            "max_errors is 0: a limit of zero findings would report every document as clean; \
             pass a positive limit"
        );
        let mut ctx = ValidationContext {
            model: &self.model,
            errors: Vec::new(),
            opts,
            path: Vec::new(),
            type_stack: Vec::new(),
        };
        ctx.validate_document(xml)?;
        Ok(ctx.errors)
    }
}

// ── Type frame ────────────────────────────────────────────────────────────────

/// The schema type of an instance element, borrowed from the model.
///
/// A reference that names no known type resolves to nothing, so it has no
/// variant here: such an element is validated as if it had no declaration.
#[derive(Clone, Copy)]
enum ResolvedType<'m> {
    Simple(&'m SimpleTypeDef),
    Complex(&'m ComplexType),
}

struct TypeFrame<'m> {
    /// Accumulated text content.
    text: String,
    /// Resolved XSD type for this element.
    xsd_type: Option<ResolvedType<'m>>,
    /// Counts of child element occurrences.
    child_counts: HashMap<String, usize>,
}

// ── Validation context ────────────────────────────────────────────────────────

struct ValidationContext<'m> {
    model: &'m SchemaModel,
    errors: Vec<ValidationError>,
    opts: ValidateOptions,
    path: Vec<String>,
    type_stack: Vec<TypeFrame<'m>>,
}

impl<'m> ValidationContext<'m> {
    fn validate_document(&mut self, xml: &[u8]) -> Result<()> {
        let mut reader = Reader::from_reader(xml);
        // Text is not trimmed as it is read. An element's value can arrive in
        // several pieces (text, a CDATA section, more text), and trimming each
        // piece would delete the spaces between them. The whole value is
        // trimmed once, at the end tag.
        reader.config_mut().trim_text(false);
        // A bare `&` is not well-formed, but it is read as the text it is, so
        // the value is checked as written and the rest of the document is
        // still validated.
        reader.config_mut().allow_dangling_amp = true;
        let mut buf = Vec::with_capacity(4096);

        // The loop ends with whether it stopped at the finding limit, which
        // leaves the rest of the document unread.
        let stopped_at_limit = loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let local = {
                        let name_bytes = e.name();
                        let raw = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("?");
                        strip_prefix(raw).to_owned()
                    };
                    self.path.push(local.clone());

                    let xsd_type = self.resolve_child_type(&local);

                    if let Some(parent) = self.type_stack.last_mut() {
                        *parent.child_counts.entry(local).or_insert(0) += 1;
                    }

                    self.type_stack.push(TypeFrame {
                        text: String::new(),
                        xsd_type,
                        child_counts: HashMap::new(),
                    });
                }
                Ok(Event::Empty(ref e)) => {
                    let local = {
                        let name_bytes = e.name();
                        let raw = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("?");
                        strip_prefix(raw).to_owned()
                    };
                    self.path.push(local.clone());

                    let xsd_type = self.resolve_child_type(&local);

                    if let Some(parent) = self.type_stack.last_mut() {
                        *parent.child_counts.entry(local).or_insert(0) += 1;
                    }

                    // A self-closing element is an element with no text and
                    // no children, and is checked as one.
                    if let Some(t) = xsd_type {
                        if self.opts.check_facets {
                            self.check_text_for_type_val(t, "");
                        }
                        if self.opts.check_cardinality {
                            self.check_cardinality_for_type(t, &HashMap::new());
                        }
                    }

                    self.path.pop();

                    if self.errors.len() >= self.opts.max_errors {
                        break true;
                    }
                }
                // Text arrives without its references, which the reader
                // reports as events of their own, in document order.
                Ok(Event::Text(ref t)) => {
                    if let Ok(text) = t.decode() {
                        if let Some(frame) = self.type_stack.last_mut() {
                            frame.text.push_str(&text);
                        }
                    }
                }
                Ok(Event::GeneralRef(ref reference)) => {
                    if let Some(frame) = self.type_stack.last_mut() {
                        push_reference(&mut frame.text, reference);
                    }
                }
                // A CDATA section is element text written literally: it is
                // decoded but never entity-unescaped.
                Ok(Event::CData(ref data)) => {
                    if let Some(frame) = self.type_stack.last_mut() {
                        frame.text.push_str(&String::from_utf8_lossy(data));
                    }
                }
                Ok(Event::End(_)) => {
                    if let Some(frame) = self.type_stack.pop() {
                        if let Some(t) = frame.xsd_type {
                            if self.opts.check_facets {
                                self.check_text_for_type_val(t, frame.text.trim());
                            }
                            if self.opts.check_cardinality {
                                self.check_cardinality_for_type(t, &frame.child_counts);
                            }
                        }
                    }
                    self.path.pop();

                    if self.errors.len() >= self.opts.max_errors {
                        break true;
                    }
                }
                Ok(Event::Eof) => break false,
                Err(e) => {
                    self.push_parse_error(&e);
                    break false;
                }
                _ => {}
            }
            buf.clear();
        };

        if stopped_at_limit {
            buf.clear();
            self.report_ill_formed_rest(&mut reader, &mut buf);
        }
        Ok(())
    }

    /// Reads the rest of a document whose findings reached the limit.
    ///
    /// Nothing more is checked against the schema, but a parse error must be
    /// reported whatever the limit, so if the reader raises one it is
    /// recorded. The element path is kept up to date for it.
    fn report_ill_formed_rest(&mut self, reader: &mut Reader<&[u8]>, buf: &mut Vec<u8>) {
        loop {
            match reader.read_event_into(buf) {
                Ok(Event::Start(ref e)) => {
                    let name_bytes = e.name();
                    let raw = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("?");
                    self.path.push(strip_prefix(raw).to_owned());
                }
                Ok(Event::End(_)) => {
                    self.path.pop();
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    self.push_parse_error(&e);
                    break;
                }
                _ => {}
            }
            buf.clear();
        }
    }

    /// Resolve the XSD type for a child element given the current type stack.
    fn resolve_child_type(&self, child_local: &str) -> Option<ResolvedType<'m>> {
        if let Some(parent) = self.type_stack.last() {
            let ResolvedType::Complex(parent_type) = parent.xsd_type? else {
                return None;
            };
            return self.find_child_in_content(&parent_type.content, child_local);
        }
        // Root element — global lookup.
        let model = self.model;
        let decl = model.global_elements.get(child_local)?;
        self.resolve_xsd_type(&decl.type_def)
    }

    /// Find a child element type within a content model, descending into nested
    /// model groups (e.g. an `xs:choice` inside an `xs:sequence`).
    ///
    /// The recursion follows group nesting, which the parser bounds.
    fn find_child_in_content(
        &self,
        content: &'m ContentModel,
        child_name: &str,
    ) -> Option<ResolvedType<'m>> {
        let model = self.model;
        let particles = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
            _ => return None,
        };
        for particle in particles {
            match &particle.element {
                ElementRef::Inline(decl) if decl.name.local == child_name => {
                    return self.resolve_xsd_type(&decl.as_ref().type_def);
                }
                ElementRef::Ref(qn) if qn.local == child_name => {
                    if let Some(decl) = model.global_elements.get(&qn.local) {
                        return self.resolve_xsd_type(&decl.type_def);
                    }
                }
                ElementRef::Group(group) => {
                    if let Some(found) = self.find_child_in_content(group, child_name) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Resolve a declared type to the definition it names, without copying it.
    fn resolve_xsd_type(&self, xsd: &'m XsdType) -> Option<ResolvedType<'m>> {
        let model = self.model;
        match xsd {
            XsdType::Simple(st) => Some(ResolvedType::Simple(st)),
            XsdType::Complex(ct) => Some(ResolvedType::Complex(ct)),
            XsdType::Ref(qn) => {
                if let Some(ct) = model.complex_types.get(&qn.local) {
                    return Some(ResolvedType::Complex(ct));
                }
                model.simple_types.get(&qn.local).map(ResolvedType::Simple)
            }
        }
    }

    fn check_text_for_type_val(&mut self, xsd: ResolvedType<'m>, text: &str) {
        let st: &'m SimpleTypeDef = match xsd {
            ResolvedType::Simple(st) => st,
            ResolvedType::Complex(ct) => {
                if let ContentModel::SimpleContent(st) = &ct.content {
                    st.as_ref()
                } else {
                    return;
                }
            }
        };
        let facets = &st.facets;
        let kind = &st.kind;

        if !facets.enumerations.is_empty() && !facets.enumerations.iter().any(|e| e.value == text) {
            let allowed = enumeration_listing(facets.enumerations.iter().map(|e| e.value.as_str()));
            self.push_error(
                ValidationSeverity::Error,
                format!("value {text:?} not in enumeration [{allowed}]"),
            );
        }

        let char_len = text.chars().count() as u32;
        if let Some(min) = facets.min_length {
            if char_len < min {
                self.push_error(
                    ValidationSeverity::Error,
                    format!("value length {char_len} below minLength {min}"),
                );
            }
        }
        if let Some(max) = facets.max_length {
            if char_len > max {
                self.push_error(
                    ValidationSeverity::Error,
                    format!("value length {char_len} exceeds maxLength {max}"),
                );
            }
        }

        if kind.is_numeric() {
            if let Ok(num) = text.parse::<f64>() {
                if let Some(min_s) = &facets.min_inclusive {
                    if let Ok(min_v) = min_s.parse::<f64>() {
                        if num < min_v {
                            self.push_error(
                                ValidationSeverity::Error,
                                format!("value {num} below minInclusive {min_v}"),
                            );
                        }
                    }
                }
                if let Some(max_s) = &facets.max_inclusive {
                    if let Ok(max_v) = max_s.parse::<f64>() {
                        if num > max_v {
                            self.push_error(
                                ValidationSeverity::Error,
                                format!("value {num} exceeds maxInclusive {max_v}"),
                            );
                        }
                    }
                }
            } else if !text.is_empty() {
                self.push_error(
                    ValidationSeverity::Warning,
                    format!("expected numeric value for {:?}, got {:?}", kind, text),
                );
            }
        }
    }

    fn check_cardinality_for_type(
        &mut self,
        xsd: ResolvedType<'m>,
        child_counts: &HashMap<String, usize>,
    ) {
        if let ResolvedType::Complex(ct) = xsd {
            self.check_cardinality_for_content(&ct.content, child_counts);
        }
    }

    /// Validate occurrence constraints for one content model.
    ///
    /// `Sequence`/`All` apply each particle's own bounds. `Choice` is a
    /// discriminated union: exactly/at-least one branch must appear (subject to
    /// the choice's own occurrence), so individual branches are never reported
    /// as "missing". A nested choice surfaces as a [`ElementRef::Group`] and is
    /// validated with the same union rule.
    ///
    /// The recursion follows group nesting, which the parser bounds. The walk
    /// stops as soon as the finding limit is reached, so a type with many
    /// required children costs at most that many findings.
    fn check_cardinality_for_content(
        &mut self,
        content: &'m ContentModel,
        child_counts: &HashMap<String, usize>,
    ) {
        let (particles, is_choice) = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) => (p, false),
            ContentModel::Choice(p) => (p, true),
            _ => return,
        };

        let mut branch_total = 0usize;
        for particle in particles {
            if self.errors.len() >= self.opts.max_errors {
                return;
            }
            // A nested model group (typically a choice) is handled as a union:
            // its branches count toward the parent only as the group's own.
            let (child_name, occ) = match &particle.element {
                ElementRef::Inline(decl) => (decl.name.local.as_str(), decl.occurrence),
                ElementRef::Ref(qn) => (qn.local.as_str(), particle.occurrence),
                ElementRef::Group(group) => {
                    let selected = group_branch_count(group, child_counts);
                    branch_total += selected;
                    if particle.occurrence.min >= 1 && selected == 0 {
                        self.push_error(
                            ValidationSeverity::Error,
                            "required choice group has no selected branch".to_string(),
                        );
                    }
                    self.check_cardinality_for_content(group, child_counts);
                    continue;
                }
            };

            let count = child_counts.get(child_name).copied().unwrap_or(0);
            branch_total += count;
            // In a choice, no single branch is individually required — only the
            // union is. Skip the per-branch required check there.
            if !is_choice && occ.min >= 1 && count == 0 {
                self.push_error(
                    ValidationSeverity::Error,
                    format!("required child <{child_name}> is missing"),
                );
            }
            if let Some(m) = occ.max {
                if count > m as usize {
                    self.push_error(
                        ValidationSeverity::Error,
                        format!("child <{child_name}> appears {count} times, max is {m}"),
                    );
                }
            }
        }

        // For a required top-level choice, at least one branch must appear.
        if is_choice && branch_total == 0 && !particles.is_empty() {
            self.push_error(
                ValidationSeverity::Error,
                "choice requires exactly one branch, none present".to_string(),
            );
        }
    }

    /// Records a finding unless [`ValidateOptions::max_errors`] is reached.
    ///
    /// Each finding carries the full element path, so the limit is what keeps
    /// the findings of a deep document bounded.
    fn push_error(&mut self, severity: ValidationSeverity, message: String) {
        if self.errors.len() >= self.opts.max_errors {
            return;
        }
        self.errors.push(ValidationError {
            severity,
            path: self.current_path(),
            message,
        });
    }

    /// Records a parse error raised by the reader, whatever the limit.
    ///
    /// Reading stops at the first parse error, so this adds at most one
    /// finding beyond [`ValidateOptions::max_errors`].
    fn push_parse_error(&mut self, error: &quick_xml::Error) {
        self.errors.push(ValidationError {
            severity: ValidationSeverity::Error,
            path: self.current_path(),
            message: format!("XML parse error: {error}"),
        });
    }

    fn current_path(&self) -> String {
        if self.path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.path.join("/"))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn strip_prefix(name: &str) -> &str {
    match name.find(':') {
        Some(pos) => &name[pos + 1..],
        None => name,
    }
}

/// Appends what one `&...;` reference in element text stands for.
///
/// A predefined entity (`&amp;`) or a decimal or hex character reference
/// (`&#233;`, `&#xE9;`) becomes its character. Anything else, an entity this
/// reader does not know or a character reference to no character (`&#0;`), is
/// kept as it was written, so the value is refused for what it holds instead
/// of silently losing part of itself.
fn push_reference(text: &mut String, reference: &BytesRef<'_>) {
    match reference.resolve_char_ref() {
        Ok(Some(character)) => {
            text.push(character);
            return;
        }
        Ok(None) => {
            let resolved = reference
                .decode()
                .ok()
                .and_then(|name| resolve_predefined_entity(&name));
            if let Some(resolved) = resolved {
                text.push_str(resolved);
                return;
            }
        }
        Err(_) => {}
    }
    text.push('&');
    text.push_str(&String::from_utf8_lossy(reference));
    text.push(';');
}

/// Joins allowed enumeration values for a finding, comma separated.
///
/// Values are quoted in full until [`MAX_ENUMERATION_LISTING_BYTES`] is
/// reached; the rest are replaced by a count. The first value is always
/// attempted, and a value that does not fit is never cut in the middle.
fn enumeration_listing<'a>(values: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = values.len();
    let mut listing = String::new();
    for (index, value) in values.enumerate() {
        let separator = if index == 0 { 0 } else { 2 };
        if listing.len() + separator + value.len() > MAX_ENUMERATION_LISTING_BYTES {
            let omitted = total - index;
            if index > 0 {
                listing.push_str(", ");
            }
            listing.push_str(&format!("... {omitted} more"));
            break;
        }
        if index > 0 {
            listing.push_str(", ");
        }
        listing.push_str(value);
    }
    listing
}

/// Total instance occurrences across every immediate branch of a model group.
///
/// Used to decide whether a (possibly required) `xs:choice` group selected any
/// branch. Only direct branches are counted; deeper nesting is validated
/// separately by recursion in the caller.
fn group_branch_count(content: &ContentModel, child_counts: &HashMap<String, usize>) -> usize {
    let particles = match content {
        ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
        _ => return 0,
    };
    particles
        .iter()
        .map(|particle| match &particle.element {
            ElementRef::Inline(decl) => child_counts.get(&decl.name.local).copied().unwrap_or(0),
            ElementRef::Ref(qn) => child_counts.get(&qn.local).copied().unwrap_or(0),
            ElementRef::Group(inner) => group_branch_count(inner, child_counts),
        })
        .sum()
}
