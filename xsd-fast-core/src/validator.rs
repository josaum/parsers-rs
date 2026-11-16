//! XML validator that uses the parsed [`SchemaModel`] to check documents.
//!
//! Validates simple-type constraints: enumerations, string length, numeric range.
//! Complex structure checks (required children, cardinality) are also included.
//!
//! Element frames borrow resolved schema types; validation never clones the
//! schema tree for each occurrence of an element.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::parser::SchemaSet;
use crate::schema::{
    ContentModel, ElementRef, SchemaModel, SchemaTypeRef, SimpleTypeDef, SimpleTypeKind, XsdType,
};

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
    /// Stop after this many errors. Default: 100.
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
    pub fn validate(&self, xml: &[u8], opts: ValidateOptions) -> Result<Vec<ValidationError>> {
        ensure!(opts.max_errors > 0, "max_errors must be greater than zero");
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

struct TypeFrame<'m> {
    /// Accumulated text content.
    text: String,
    /// Borrowed resolved schema type for this element.
    xsd_type: Option<SchemaTypeRef<'m>>,
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
        reader.config_mut().trim_text(false);
        reader.config_mut().expand_empty_elements = true;
        let mut roots = 0usize;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    // Expand empty elements into start/end events below via
                    // the reader configuration, so both forms use one path.
                    if self.type_stack.is_empty() {
                        roots += 1;
                        if roots > 1 {
                            self.push_error(
                                ValidationSeverity::Error,
                                "XML has multiple root elements".into(),
                            );
                            break;
                        }
                    }
                    let name = e.name();
                    let local = match std::str::from_utf8(name.as_ref()) {
                        Ok(name) => strip_prefix(name),
                        Err(error) => {
                            self.push_error(
                                ValidationSeverity::Error,
                                format!("invalid XML name: {error}"),
                            );
                            break;
                        }
                    };
                    let xsd_type = self.resolve_child_type(local);
                    self.path.push(local.to_owned());
                    if xsd_type.is_none() {
                        self.push_error(
                            ValidationSeverity::Error,
                            format!("no schema type for element <{local}>"),
                        );
                    }
                    if let Some(parent) = self.type_stack.last_mut() {
                        *parent.child_counts.entry(local.to_owned()).or_insert(0) += 1;
                    }
                    self.type_stack.push(TypeFrame {
                        text: String::new(),
                        xsd_type,
                        child_counts: HashMap::new(),
                    });
                }
                Ok(Event::Text(t)) => match t.unescape() {
                    Ok(text) => self.append_text(&text, false),
                    Err(error) => self.push_error(
                        ValidationSeverity::Error,
                        format!("XML text error: {error}"),
                    ),
                },
                Ok(Event::CData(t)) => match reader.decoder().decode(t.as_ref()) {
                    Ok(text) => self.append_text(&text, true),
                    Err(error) => self.push_error(
                        ValidationSeverity::Error,
                        format!("XML CDATA error: {error}"),
                    ),
                },
                Ok(Event::End(_)) => {
                    if let Some(frame) = self.type_stack.pop() {
                        if let Some(t) = frame.xsd_type {
                            if self.opts.check_facets {
                                self.check_text_for_type_val(t, &frame.text);
                            }
                            if self.opts.check_cardinality {
                                self.check_cardinality_for_type(t, &frame.child_counts);
                            }
                        }
                    }
                    self.path.pop();
                }
                Ok(Event::Eof) => {
                    if roots == 0 {
                        self.push_error(
                            ValidationSeverity::Error,
                            "XML document has no root element".into(),
                        );
                    } else if !self.type_stack.is_empty() {
                        self.push_error(
                            ValidationSeverity::Error,
                            "XML document ends before its elements are closed".into(),
                        );
                    }
                    break;
                }
                Err(error) => {
                    self.push_error(
                        ValidationSeverity::Error,
                        format!("XML parse error: {error}"),
                    );
                    break;
                }
                _ => {}
            }
            if self.errors.len() >= self.opts.max_errors {
                break;
            }
        }
        Ok(())
    }

    fn append_text(&mut self, text: &str, cdata: bool) {
        if let Some(frame) = self.type_stack.last_mut() {
            frame.text.push_str(text);
        } else if cdata || !text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
            self.push_error(
                ValidationSeverity::Error,
                "XML content outside the root element".into(),
            );
        }
    }

    /// Resolve the XSD type for a child element given the current type stack.
    fn resolve_child_type(&self, child_local: &str) -> Option<SchemaTypeRef<'m>> {
        if let Some(parent) = self.type_stack.last() {
            let parent_type = parent.xsd_type?;
            return self.find_child_xsd_type(parent_type, child_local);
        }
        // Root element — global lookup.
        let decl = self.model.global_elements.get(child_local)?;
        self.resolve_xsd_type(&decl.type_def)
    }

    /// Find the child element type inside a parent XsdType.
    fn find_child_xsd_type(
        &self,
        parent: SchemaTypeRef<'m>,
        child_name: &str,
    ) -> Option<SchemaTypeRef<'m>> {
        let SchemaTypeRef::Complex(ct) = parent else {
            return None;
        };
        self.find_child_in_content(&ct.content, child_name)
    }

    /// Find a child element type within a content model, descending into nested
    /// model groups (e.g. an `xs:choice` inside an `xs:sequence`).
    fn find_child_in_content(
        &self,
        content: &'m ContentModel,
        child_name: &str,
    ) -> Option<SchemaTypeRef<'m>> {
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
                    if let Some(decl) = self.model.global_elements.get(&qn.local) {
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

    fn resolve_xsd_type(&self, xsd: &'m XsdType) -> Option<SchemaTypeRef<'m>> {
        match xsd {
            XsdType::Ref(qn) => self.model.resolve_type(&qn.local),
            XsdType::Simple(st) => Some(SchemaTypeRef::Simple(st)),
            XsdType::Complex(ct) => Some(SchemaTypeRef::Complex(ct)),
        }
    }

    fn check_text_for_type_val(&mut self, xsd: SchemaTypeRef<'m>, text: &str) {
        let st: Option<&SimpleTypeDef> = match xsd {
            SchemaTypeRef::Simple(st) => Some(st),
            SchemaTypeRef::Complex(ct) => match &ct.content {
                ContentModel::SimpleContent(st) => Some(st),
                _ => None,
            },
        };
        let st = match st {
            Some(s) => s,
            None => return,
        };
        let normalized = normalize_whitespace(text, &st.kind);
        let text = normalized.as_ref();
        let facets = &st.facets;
        let kind = &st.kind;

        if !facets.enumerations.is_empty() && !facets.enumerations.iter().any(|e| e.value == text) {
            let allowed: Vec<&str> = facets
                .enumerations
                .iter()
                .map(|e| e.value.as_str())
                .collect();
            self.push_error(
                ValidationSeverity::Error,
                format!(
                    "value {:?} not in enumeration [{}]",
                    text,
                    allowed.join(", ")
                ),
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
        xsd: SchemaTypeRef<'m>,
        child_counts: &HashMap<String, usize>,
    ) {
        if let SchemaTypeRef::Complex(ct) = xsd {
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
    fn check_cardinality_for_content(
        &mut self,
        content: &ContentModel,
        child_counts: &HashMap<String, usize>,
    ) {
        let (particles, is_choice) = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) => (p, false),
            ContentModel::Choice(p) => (p, true),
            _ => return,
        };

        let mut branch_total = 0usize;
        for particle in particles {
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
            if !is_choice && count < occ.min as usize {
                self.push_error(
                    ValidationSeverity::Error,
                    format!(
                        "child <{child_name}> appears {count} times, min is {}",
                        occ.min
                    ),
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

// XSD built-in whitespace normalization uses XML whitespace, not Unicode trim.
fn normalize_whitespace<'a>(text: &'a str, kind: &SimpleTypeKind) -> Cow<'a, str> {
    if matches!(kind, SimpleTypeKind::String | SimpleTypeKind::Other(_)) {
        return Cow::Borrowed(text);
    }
    if matches!(kind, SimpleTypeKind::NormalizedString) {
        return if text.contains(['\t', '\r', '\n']) {
            Cow::Owned(text.replace(['\t', '\r', '\n'], " "))
        } else {
            Cow::Borrowed(text)
        };
    }
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    if !trimmed.contains(['\t', '\r', '\n']) && !trimmed.contains("  ") {
        return Cow::Borrowed(trimmed);
    }
    Cow::Owned(
        trimmed
            .split([' ', '\t', '\r', '\n'])
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}
