//! XML validator that uses the parsed [`SchemaModel`] to check documents.
//!
//! Validates simple-type constraints: enumerations, string length, numeric range.
//! Complex structure checks (required children, cardinality) are also included.
//!
//! Each [`TypeFrame`] on the stack owns a cloned `XsdType` so there are no
//! lifetime conflicts between the immutable schema model reference and the
//! mutable error accumulation.

use std::collections::HashMap;

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::parser::SchemaSet;
use crate::schema::{ComplexType, ContentModel, ElementRef, SchemaModel, SimpleTypeDef, XsdType};

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

struct TypeFrame {
    /// Accumulated text content.
    text: String,
    /// Owned clone of the resolved XSD type for this element.
    xsd_type: Option<XsdType>,
    /// Counts of child element occurrences.
    child_counts: HashMap<String, usize>,
}

// ── Validation context ────────────────────────────────────────────────────────

struct ValidationContext<'m> {
    model: &'m SchemaModel,
    errors: Vec<ValidationError>,
    opts: ValidateOptions,
    path: Vec<String>,
    type_stack: Vec<TypeFrame>,
}

impl<'m> ValidationContext<'m> {
    fn validate_document(&mut self, xml: &[u8]) -> Result<()> {
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::with_capacity(4096);

        loop {
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

                    if let Some(parent) = self.type_stack.last_mut() {
                        *parent.child_counts.entry(local.clone()).or_insert(0) += 1;
                    }

                    let xsd_type = self.resolve_child_type(&local);
                    if self.opts.check_facets {
                        if let Some(ref t) = xsd_type {
                            self.check_text_for_type_val(t, "");
                        }
                    }

                    self.path.pop();

                    if self.errors.len() >= self.opts.max_errors {
                        break;
                    }
                }
                Ok(Event::Text(ref t)) => {
                    if let Ok(text) = t.unescape() {
                        if let Some(frame) = self.type_stack.last_mut() {
                            frame.text.push_str(&text);
                        }
                    }
                }
                Ok(Event::End(_)) => {
                    if let Some(frame) = self.type_stack.pop() {
                        let text = frame.text.clone();
                        let counts = frame.child_counts.clone();
                        let xsd_type = frame.xsd_type.clone();

                        if self.opts.check_facets {
                            if let Some(ref t) = xsd_type {
                                self.check_text_for_type_val(t, &text);
                            }
                        }
                        if self.opts.check_cardinality {
                            if let Some(ref t) = xsd_type {
                                self.check_cardinality_for_type(t, &counts);
                            }
                        }
                    }
                    self.path.pop();

                    if self.errors.len() >= self.opts.max_errors {
                        break;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    self.push_error(ValidationSeverity::Error, format!("XML parse error: {e}"));
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(())
    }

    /// Resolve the XSD type for a child element given the current type stack.
    fn resolve_child_type(&self, child_local: &str) -> Option<XsdType> {
        if let Some(parent) = self.type_stack.last() {
            let parent_type = parent.xsd_type.as_ref()?;
            return self.find_child_xsd_type(parent_type, child_local);
        }
        // Root element — global lookup.
        let decl = self.model.global_elements.get(child_local)?;
        Some(self.resolve_xsd_type(&decl.type_def))
    }

    /// Find the child element type inside a parent XsdType.
    fn find_child_xsd_type(&self, parent: &XsdType, child_name: &str) -> Option<XsdType> {
        let ct = self.as_complex_type(parent)?;
        self.find_child_in_content(&ct.content, child_name)
    }

    /// Find a child element type within a content model, descending into nested
    /// model groups (e.g. an `xs:choice` inside an `xs:sequence`).
    fn find_child_in_content(&self, content: &ContentModel, child_name: &str) -> Option<XsdType> {
        let particles = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
            _ => return None,
        };
        for particle in particles {
            match &particle.element {
                ElementRef::Inline(decl) if decl.name.local == child_name => {
                    return Some(self.resolve_xsd_type(&decl.as_ref().type_def));
                }
                ElementRef::Ref(qn) if qn.local == child_name => {
                    if let Some(decl) = self.model.global_elements.get(&qn.local) {
                        return Some(self.resolve_xsd_type(&decl.type_def));
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

    /// Resolve a Ref to its concrete type; return the type unchanged otherwise.
    fn resolve_xsd_type(&self, xsd: &XsdType) -> XsdType {
        match xsd {
            XsdType::Ref(qn) => {
                if let Some(ct) = self.model.complex_types.get(&qn.local) {
                    return XsdType::Complex(ct.clone());
                }
                if let Some(st) = self.model.simple_types.get(&qn.local) {
                    return XsdType::Simple(st.clone());
                }
                xsd.clone()
            }
            other => other.clone(),
        }
    }

    /// Return a reference to the complex type inside an XsdType if resolvable.
    fn as_complex_type<'a>(&'a self, xsd: &'a XsdType) -> Option<&'a ComplexType> {
        match xsd {
            XsdType::Complex(ct) => Some(ct),
            XsdType::Ref(qn) => self.model.complex_types.get(&qn.local),
            _ => None,
        }
    }

    fn check_text_for_type_val(&mut self, xsd: &XsdType, text: &str) {
        let st: Option<&SimpleTypeDef> = match xsd {
            XsdType::Simple(st) => Some(st),
            XsdType::Ref(qn) => self.model.simple_types.get(&qn.local),
            XsdType::Complex(ct) => {
                if let ContentModel::SimpleContent(st) = &ct.content {
                    Some(st.as_ref())
                } else {
                    None
                }
            }
        };
        let st = match st {
            Some(s) => s,
            None => return,
        };
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

    fn check_cardinality_for_type(&mut self, xsd: &XsdType, child_counts: &HashMap<String, usize>) {
        let content = match self.as_complex_type(xsd) {
            Some(ct) => ct.content.clone(),
            None => return,
        };
        self.check_cardinality_for_content(&content, child_counts);
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
                ElementRef::Inline(decl) => (decl.name.local.clone(), decl.occurrence),
                ElementRef::Ref(qn) => (qn.local.clone(), particle.occurrence),
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

            let count = child_counts.get(&child_name).copied().unwrap_or(0);
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

    fn push_error(&mut self, severity: ValidationSeverity, message: String) {
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
