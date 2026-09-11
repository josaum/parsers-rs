//! Core XSD type model.
//!
//! This module defines the in-memory representation of a parsed XSD schema.
//! Only the subset needed for Arrow derivation and simple-type validation is modelled.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Qualified name: namespace URI (optional) + local name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QName {
    /// Namespace URI associated with the declaration.
    pub namespace: Option<String>,
    /// Local part, e.g. `"mensagemschema"`.
    pub local: String,
}

impl QName {
    pub(crate) fn local(name: impl Into<String>) -> Self {
        Self {
            namespace: None,
            local: name.into(),
        }
    }
}

impl std::fmt::Display for QName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(ns) => write!(f, "{{{}}}{}", ns, self.local),
            None => write!(f, "{}", self.local),
        }
    }
}

/// Occurrence bounds on an element or particle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceConstraint {
    /// Minimum number of times the element or particle must appear.
    pub min: u32,
    /// `None` means `unbounded`.
    pub max: Option<u32>,
}

impl Default for OccurrenceConstraint {
    fn default() -> Self {
        Self {
            min: 1,
            max: Some(1),
        }
    }
}

impl OccurrenceConstraint {
    /// Return `true` when the occurrence requires at least one value.
    pub fn is_required(&self) -> bool {
        self.min >= 1
    }

    /// Return `true` when the occurrence can produce multiple values.
    pub fn is_multi(&self) -> bool {
        self.max.is_none_or(|m| m > 1)
    }
}

/// A single allowed enumeration value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumValue {
    /// Lexical value allowed by the enumeration facet.
    pub value: String,
    /// Optional human-readable documentation attached to the value.
    pub documentation: Option<String>,
}

/// Facet-level constraints for simple types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimpleFacets {
    /// Allowed lexical values from `xs:enumeration`.
    pub enumerations: Vec<EnumValue>,
    /// Regex pattern (XSD dialect).
    pub pattern: Option<String>,
    /// Minimum character length from `xs:minLength`.
    pub min_length: Option<u32>,
    /// Maximum character length from `xs:maxLength`.
    pub max_length: Option<u32>,
    /// Inclusive lower lexical bound from `xs:minInclusive`.
    pub min_inclusive: Option<String>,
    /// Inclusive upper lexical bound from `xs:maxInclusive`.
    pub max_inclusive: Option<String>,
    /// Exclusive lower lexical bound from `xs:minExclusive`.
    pub min_exclusive: Option<String>,
    /// Exclusive upper lexical bound from `xs:maxExclusive`.
    pub max_exclusive: Option<String>,
    /// Maximum total decimal digits from `xs:totalDigits`.
    pub total_digits: Option<u32>,
    /// Maximum decimal fraction digits from `xs:fractionDigits`.
    pub fraction_digits: Option<u32>,
}

impl SimpleFacets {
    /// Return `true` when no supported validation facets are present.
    pub fn is_empty(&self) -> bool {
        self.enumerations.is_empty()
            && self.pattern.is_none()
            && self.min_length.is_none()
            && self.max_length.is_none()
            && self.min_inclusive.is_none()
            && self.max_inclusive.is_none()
    }
}

/// The primitive kind of a simple XSD type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimpleTypeKind {
    /// `xs:string`.
    String,
    /// `xs:normalizedString`.
    NormalizedString,
    /// `xs:token`.
    Token,
    /// `xs:boolean`.
    Boolean,
    /// Integer-like XSD numeric types not mapped more specifically.
    Integer,
    /// `xs:long`.
    Long,
    /// `xs:int`.
    Int,
    /// `xs:short`.
    Short,
    /// `xs:byte`.
    Byte,
    /// `xs:unsignedLong`.
    UnsignedLong,
    /// `xs:unsignedInt`.
    UnsignedInt,
    /// `xs:unsignedShort`.
    UnsignedShort,
    /// `xs:unsignedByte`.
    UnsignedByte,
    /// `xs:decimal`.
    Decimal,
    /// `xs:float`.
    Float,
    /// `xs:double`.
    Double,
    /// `xs:date`.
    Date,
    /// `xs:time`.
    Time,
    /// `xs:dateTime`.
    DateTime,
    /// `xs:base64Binary`.
    Base64Binary,
    /// `xs:hexBinary`.
    HexBinary,
    /// `xs:anyURI`.
    AnyUri,
    /// Anything else — treated as string at the Arrow layer.
    Other(String),
}

impl SimpleTypeKind {
    pub(crate) fn from_local(local: &str) -> Self {
        match local {
            "string" => Self::String,
            "normalizedString" => Self::NormalizedString,
            "token" => Self::Token,
            "boolean" => Self::Boolean,
            "integer" | "nonNegativeInteger" | "positiveInteger" | "negativeInteger"
            | "nonPositiveInteger" => Self::Integer,
            "long" => Self::Long,
            "int" => Self::Int,
            "short" => Self::Short,
            "byte" => Self::Byte,
            "unsignedLong" => Self::UnsignedLong,
            "unsignedInt" => Self::UnsignedInt,
            "unsignedShort" => Self::UnsignedShort,
            "unsignedByte" => Self::UnsignedByte,
            "decimal" => Self::Decimal,
            "float" => Self::Float,
            "double" => Self::Double,
            "date" => Self::Date,
            "time" => Self::Time,
            "dateTime" => Self::DateTime,
            "base64Binary" => Self::Base64Binary,
            "hexBinary" => Self::HexBinary,
            "anyURI" => Self::AnyUri,
            other => Self::Other(other.to_string()),
        }
    }

    /// Returns `true` if this type is numeric.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::Integer
                | Self::Long
                | Self::Int
                | Self::Short
                | Self::Byte
                | Self::UnsignedLong
                | Self::UnsignedInt
                | Self::UnsignedShort
                | Self::UnsignedByte
                | Self::Decimal
                | Self::Float
                | Self::Double
        )
    }
}

/// A resolved XSD type (simple or complex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum XsdType {
    /// Inline or resolved simple type.
    Simple(SimpleTypeDef),
    /// Inline or resolved complex type.
    Complex(ComplexType),
    /// Unresolved forward reference — filled in after full parse.
    Ref(QName),
}

/// A named simple type with its primitive kind and facets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleTypeDef {
    /// Optional qualified name for named simple types.
    pub name: Option<QName>,
    /// Primitive base kind (after chasing the restriction chain).
    pub kind: SimpleTypeKind,
    /// Supported restriction facets.
    pub facets: SimpleFacets,
}

impl SimpleTypeDef {
    pub(crate) fn string_unbounded() -> Self {
        Self {
            name: None,
            kind: SimpleTypeKind::String,
            facets: SimpleFacets::default(),
        }
    }
}

/// Content model variants for complex types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentModel {
    /// `xs:sequence` — ordered list of particles.
    Sequence(Vec<Particle>),
    /// `xs:all` — unordered set, each appears at most once.
    All(Vec<Particle>),
    /// `xs:choice` — exactly one of the listed particles.
    Choice(Vec<Particle>),
    /// Simple content with a text value (e.g. `xs:extension base="xs:string"`).
    SimpleContent(Box<SimpleTypeDef>),
    /// Empty content (no children, no text).
    Empty,
}

/// A complex type: attributes + content model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexType {
    /// Optional qualified name for named complex types.
    pub name: Option<QName>,
    /// Child element and text content structure.
    ///
    /// After [`SchemaModel::resolve_inheritance`] runs, this holds the
    /// *effective* content: the base type's particles (when [`base`] is set)
    /// prepended ahead of this type's own. Before resolution, or for types that
    /// do not inherit, it holds only the locally declared particles.
    ///
    /// [`base`]: ComplexType::base
    pub content: ContentModel,
    /// Attribute declarations attached to this complex type.
    ///
    /// After [`SchemaModel::resolve_inheritance`] runs, inherited attributes are
    /// prepended ahead of locally declared ones (for inheriting types only).
    pub attributes: Vec<AttributeDecl>,
    /// Base type for `complexContent`/`extension` against a *complex* base.
    ///
    /// `None` for types that do not derive from a complex base (the common
    /// case). When set, [`SchemaModel::resolve_inheritance`] uses it to fold the
    /// base's particles and attributes into [`content`] and [`attributes`].
    ///
    /// [`content`]: ComplexType::content
    /// [`attributes`]: ComplexType::attributes
    #[serde(default)]
    pub base: Option<QName>,
}

impl ComplexType {
    /// Effective content model, including any content folded in from a base type.
    ///
    /// After [`SchemaModel::resolve_inheritance`] has run, the stored
    /// [`content`](ComplexType::content) already reflects inheritance, so this
    /// returns it directly. It exists as a stable, intention-revealing accessor
    /// for callers that walk the content model.
    pub fn effective_content(&self) -> &ContentModel {
        &self.content
    }

    /// Effective attributes, including any inherited from a base type.
    ///
    /// See [`effective_content`](ComplexType::effective_content) for the
    /// resolution contract.
    pub fn effective_attributes(&self) -> &[AttributeDecl] {
        &self.attributes
    }

    /// Whether this type derives from a complex base via `complexContent`/`extension`.
    pub fn inherits(&self) -> bool {
        self.base.is_some()
    }
}

/// A particle in a content model — wraps an element reference or inline decl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Particle {
    /// Occurrence bounds for this particle.
    pub occurrence: OccurrenceConstraint,
    /// Referenced or inline element declaration.
    pub element: ElementRef,
}

/// A reference or inline declaration of an element inside a content model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ElementRef {
    /// Inline element declaration nested inside a complex type.
    Inline(Box<ElementDecl>),
    /// Reference to a global element by qualified name.
    Ref(QName),
    /// A nested model group (e.g. an `xs:choice` inside an `xs:sequence`).
    ///
    /// Keeps the discriminated-union nature of a nested `xs:choice` instead of
    /// flattening its branches into the parent sequence. The particle's own
    /// [`OccurrenceConstraint`] applies to the whole group.
    Group(Box<ContentModel>),
}

/// A top-level or nested element declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementDecl {
    /// Element qualified name.
    pub name: QName,
    /// Declared or inferred element type.
    pub type_def: XsdType,
    /// Occurrence bounds for this declaration.
    pub occurrence: OccurrenceConstraint,
    /// Whether `xsi:nil="true"` is allowed.
    pub nillable: bool,
    /// Optional human-readable documentation.
    pub documentation: Option<String>,
}

/// An attribute declaration inside a complex type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeDecl {
    /// Attribute local name.
    pub name: String,
    /// Attribute simple type.
    pub type_def: SimpleTypeDef,
    /// Whether `use="required"` is set.
    pub required: bool,
    /// Optional default lexical value.
    pub default: Option<String>,
    /// Optional fixed lexical value.
    pub fixed: Option<String>,
}

/// The resolved schema: named types + global elements.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SchemaModel {
    /// Target namespace of the primary schema file.
    pub target_namespace: Option<String>,
    /// All named complex types keyed by local name.
    pub complex_types: IndexMap<String, ComplexType>,
    /// All named simple types keyed by local name.
    pub simple_types: IndexMap<String, SimpleTypeDef>,
    /// Global element declarations keyed by local name.
    pub global_elements: IndexMap<String, ElementDecl>,
}

impl SchemaModel {
    /// Resolve a type reference by local name, searching both simple and complex type registries.
    pub fn resolve_type(&self, local: &str) -> Option<SchemaTypeRef<'_>> {
        if let Some(ct) = self.complex_types.get(local) {
            return Some(SchemaTypeRef::Complex(ct));
        }
        if let Some(st) = self.simple_types.get(local) {
            return Some(SchemaTypeRef::Simple(st));
        }
        None
    }

    /// Resolve a global element by local name.
    pub fn resolve_element(&self, local: &str) -> Option<&ElementDecl> {
        self.global_elements.get(local)
    }

    /// Fold `complexContent`/`extension` base types into derived types.
    ///
    /// For every complex type whose [`base`](ComplexType::base) is set, this
    /// prepends the (transitively) resolved base's particles and attributes
    /// ahead of the type's own. Inline complex types nested in particles are
    /// resolved too, so inheritance works regardless of where it appears. Types
    /// that do not inherit are left byte-identical.
    ///
    /// Idempotent in effect for the common case, but intended to run exactly
    /// once, immediately after a schema set finishes loading. Cycles and
    /// self-references are bounded by a visited set and a depth cap, so a
    /// malformed schema can never cause unbounded recursion.
    pub fn resolve_inheritance(&mut self) {
        // Snapshot of the *local* (pre-resolution) named types. All base
        // chasing reads from this immutable view, which makes the pass
        // order-independent and prevents double-folding when a base type is
        // itself resolved later in the same pass.
        let originals = self.complex_types.clone();

        for (name, ct) in self.complex_types.iter_mut() {
            resolve_complex_type(ct, Some(name.as_str()), &originals, 0);
        }
    }
}

/// Maximum complex-type derivation depth chased during inheritance resolution.
///
/// Type extension chains are shallow in real schemas (schema bottoms out at two
/// levels). The cap only exists to bound malformed/cyclic schemas; mirrors the
/// bounded-recursion philosophy of the Arrow derivation walk.
const MAX_INHERITANCE_DEPTH: usize = 16;

/// Resolve one complex type in place: fold its base, then recurse into inline
/// children so nested `complexContent`/`extension` is resolved as well.
///
/// `owner` is the type's own name (for named types), used to reject a base that
/// points straight back at the type itself.
fn resolve_complex_type(
    ct: &mut ComplexType,
    owner: Option<&str>,
    originals: &IndexMap<String, ComplexType>,
    depth: usize,
) {
    // Fold the base chain into this type's content + attributes. A fresh
    // visited stack (seeded with the owner) bounds cycles for this chain only.
    if let Some(base) = ct.base.clone() {
        let mut visited: Vec<String> = owner.map(|s| vec![s.to_string()]).unwrap_or_default();
        if let Some((mut base_particles, mut base_attrs)) =
            collect_base(&base.local, originals, &mut visited, depth)
        {
            if let ContentModel::Sequence(own) = &ct.content {
                base_particles.extend(own.iter().cloned());
                ct.content = ContentModel::Sequence(base_particles);
            } else if matches!(ct.content, ContentModel::Empty) {
                // Pure extension that only adds attributes: keep the base's
                // particles verbatim.
                ct.content = ContentModel::Sequence(base_particles);
            }
            // Inherited attributes come first, then locally declared ones.
            base_attrs.append(&mut ct.attributes);
            ct.attributes = base_attrs;
        }
    }

    // Recurse into inline complex types so nested extensions are resolved too.
    if depth < MAX_INHERITANCE_DEPTH {
        if let Some(particles) = content_particles_mut(&mut ct.content) {
            for particle in particles {
                resolve_element_ref(&mut particle.element, originals, depth + 1);
            }
        }
    }
}

/// Resolve any inline complex type reachable through one particle's element.
fn resolve_element_ref(
    element: &mut ElementRef,
    originals: &IndexMap<String, ComplexType>,
    depth: usize,
) {
    if depth >= MAX_INHERITANCE_DEPTH {
        return;
    }
    match element {
        ElementRef::Inline(decl) => {
            if let XsdType::Complex(inner) = &mut decl.type_def {
                // Inline types are anonymous; they have no name to self-reference.
                resolve_complex_type(inner, None, originals, depth);
            }
        }
        ElementRef::Group(group) => {
            if let Some(particles) = content_particles_mut(group) {
                for particle in particles {
                    resolve_element_ref(&mut particle.element, originals, depth + 1);
                }
            }
        }
        ElementRef::Ref(_) => {}
    }
}

/// Collect a base type's effective particles and attributes, base-first,
/// chasing the base chain transitively. Returns `None` if the base is unknown
/// or already on the current chain (cycle).
fn collect_base(
    base_local: &str,
    originals: &IndexMap<String, ComplexType>,
    visited: &mut Vec<String>,
    depth: usize,
) -> Option<(Vec<Particle>, Vec<AttributeDecl>)> {
    if depth >= MAX_INHERITANCE_DEPTH || visited.iter().any(|v| v == base_local) {
        return None;
    }
    let base = originals.get(base_local)?;
    visited.push(base_local.to_string());

    // Recurse first so deeper ancestors land before this base's own members.
    let mut particles: Vec<Particle> = Vec::new();
    let mut attrs: Vec<AttributeDecl> = Vec::new();
    if let Some(grand) = base.base.as_ref() {
        if let Some((gp, ga)) = collect_base(&grand.local, originals, visited, depth + 1) {
            particles.extend(gp);
            attrs.extend(ga);
        }
    }
    if let ContentModel::Sequence(p) = &base.content {
        particles.extend(p.iter().cloned());
    }
    attrs.extend(base.attributes.iter().cloned());

    visited.pop();
    Some((particles, attrs))
}

/// Mutable access to a content model's particles, for the variants that carry them.
fn content_particles_mut(content: &mut ContentModel) -> Option<&mut Vec<Particle>> {
    match content {
        ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => Some(p),
        _ => None,
    }
}

/// A borrowed reference to a resolved schema type.
pub enum SchemaTypeRef<'a> {
    /// Borrowed simple type reference.
    Simple(&'a SimpleTypeDef),
    /// Borrowed complex type reference.
    Complex(&'a ComplexType),
}

/// Re-exported for convenience at crate root.
pub use SchemaTypeRef as SchemaType;
