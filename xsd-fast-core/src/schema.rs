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

/// How a complex type derives from its complex base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Derivation {
    /// `xs:extension`: the base's particles and attributes, then the type's
    /// own.
    #[default]
    Extension,
    /// `xs:restriction`: the particles the type states itself, and the
    /// base's attributes except those it prohibits.
    Restriction,
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
    /// Base type of a `complexContent` extension or restriction against a
    /// *complex* base.
    ///
    /// `None` for types that do not derive from a complex base (the common
    /// case). When set, [`SchemaModel::resolve_inheritance`] uses it to fold
    /// the base into [`content`] and [`attributes`]: particles and attributes
    /// for an extension, attributes only for a restriction, as
    /// [`derivation`] says.
    ///
    /// [`content`]: ComplexType::content
    /// [`attributes`]: ComplexType::attributes
    /// [`derivation`]: ComplexType::derivation
    #[serde(default)]
    pub base: Option<QName>,
    /// How the type derives from [`base`](ComplexType::base); means nothing
    /// without one.
    #[serde(default)]
    pub derivation: Derivation,
    /// Names of the attributes a restriction declares with
    /// `use="prohibited"`.
    ///
    /// They are not attributes of the type, so they are not in
    /// [`attributes`](ComplexType::attributes); the names are kept so the
    /// base's attributes of those names are left out when it is folded in.
    #[serde(default)]
    pub prohibited_attributes: Vec<String>,
    /// Occurrence bounds of the model group that is the type's content: the
    /// `minOccurs` / `maxOccurs` written on its `xs:sequence`, `xs:all` or
    /// `xs:choice`. With a minimum of zero the whole content may be left out,
    /// required children included. Once by default, and for content that is
    /// not a model group.
    #[serde(default)]
    pub content_occurrence: OccurrenceConstraint,
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

    /// Whether this type derives from a complex base, by extension or by
    /// restriction.
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

    /// Fold `complexContent` base types into derived types.
    ///
    /// For every complex type whose [`base`](ComplexType::base) is set, this
    /// prepends the (transitively) resolved base's particles and attributes
    /// ahead of the type's own. A restriction keeps the particles it states
    /// and takes only the base's attributes: those it prohibits are left
    /// out, and one it declares again keeps the base's type. Inline complex types nested in particles, and
    /// the inline types of global elements, are resolved too, so inheritance
    /// works regardless of where it appears. A chain of bases is followed to
    /// its end, however long. Types that do not inherit are left
    /// byte-identical.
    ///
    /// A recursive type, one holding an element whose inline type extends
    /// the type itself or a type that leads back to it, is unrolled once:
    /// the inline type gets its base, and the same extension met again
    /// inside that copy keeps only what it declares itself.
    ///
    /// Idempotent in effect for the common case, but intended to run exactly
    /// once, immediately after a schema set finishes loading. A cycle is
    /// broken at the first name met twice, and the chain walk does not
    /// recurse, so a malformed schema can never cause unbounded recursion.
    ///
    /// Folding copies each base into every type derived from it, so the model
    /// can grow far beyond the schema text, and the copies are capped by a
    /// budget. **When the budget runs out this method stops quietly**: it
    /// returns as if it had finished and reports nothing. The derived types it
    /// had not reached keep only their own particles and attributes, the type
    /// it was folding may be folded at its top level but not inside its inline
    /// types, and nothing in the model marks which types were folded.
    /// Validation and Arrow derivation then treat the missing inherited
    /// children as undeclared.
    ///
    /// The form that reports this is `try_resolve_inheritance`, which is
    /// internal to this crate. [`SchemaSet::load`](crate::SchemaSet::load) and
    /// [`SchemaSet::load_bytes`](crate::SchemaSet::load_bytes) call it and
    /// return its `resource_limit` error, so load through [`SchemaSet`](crate::SchemaSet)
    /// where that failure must be seen.
    pub fn resolve_inheritance(&mut self) {
        // The only failure is the copy budget, which this signature cannot
        // report. What was folded before it ran out stays folded.
        let _ = self.try_resolve_inheritance();
    }

    /// Folds base types into derived types, failing when the copies exceed
    /// [`MAX_INHERITED_NODES`].
    ///
    /// # Errors
    ///
    /// Returns a `resource_limit` error when inheritance would copy more
    /// particles, attributes, enumeration values and text than the budget
    /// allows. The model is then partially folded and should be discarded.
    pub(crate) fn try_resolve_inheritance(&mut self) -> anyhow::Result<()> {
        // Snapshot of the *local* (pre-resolution) named types. All base
        // chasing reads from this immutable view, which makes the pass
        // order-independent and prevents double-folding when a base type is
        // itself resolved later in the same pass.
        let originals = self.complex_types.clone();
        let mut budget = InheritanceBudget {
            remaining: MAX_INHERITED_NODES,
            uncharged_text_bytes: 0,
        };

        let mut folding = Vec::new();

        for (name, ct) in self.complex_types.iter_mut() {
            resolve_complex_type(
                ct,
                Some(name.as_str()),
                &originals,
                0,
                &mut folding,
                &mut budget,
            )?;
        }
        // A global element can hold an inline type, which can extend a named
        // type itself or hold elements whose inline types do.
        for element in self.global_elements.values_mut() {
            if let XsdType::Complex(inline) = &mut element.type_def {
                resolve_complex_type(inline, None, &originals, 0, &mut folding, &mut budget)?;
            }
        }
        Ok(())
    }
}

/// Most that inheritance folding may copy in one schema set, in budget units.
///
/// A unit is one copied particle, attribute, enumeration value or
/// simple-content type, or [`TEXT_BYTES_PER_NODE`] bytes of the text they
/// own; see [`CopyCost`].
///
/// Folding is eager: every derived type receives its own copy of each
/// ancestor's content, and an inline type that extends a named type receives
/// a copy that can itself hold inline extensions. A schema of a few kilobytes
/// can therefore ask for billions of copies (six inline extensions of the
/// enclosing type, nested sixteen levels deep, is 6^16), and a base holding
/// one large enumeration is copied whole into every type derived from it.
///
/// The limit is a bound on memory. A schema set that spends all of it peaks
/// at about 130 MiB of process memory when the copies are plain element
/// particles, about 120 MiB for inline types with attributes, and about
/// 70 MiB for long names or values (the three shapes measured, on a 64-bit
/// build, loading included). Changing what a unit charges needs those
/// measurements repeated.
const MAX_INHERITED_NODES: usize = 1 << 18;

/// Copies still allowed during one [`SchemaModel::try_resolve_inheritance`] pass.
struct InheritanceBudget {
    /// Budget units left.
    remaining: usize,
    /// Copied text bytes not yet charged: always less than one unit's worth.
    uncharged_text_bytes: usize,
}

impl InheritanceBudget {
    /// Charges one copy: a unit per node, and a unit per
    /// [`TEXT_BYTES_PER_NODE`] bytes of text summed over the whole pass.
    fn charge(&mut self, cost: CopyCost) -> anyhow::Result<()> {
        let text_bytes = self.uncharged_text_bytes.saturating_add(cost.text_bytes);
        self.uncharged_text_bytes = text_bytes % TEXT_BYTES_PER_NODE;
        let units = cost.nodes.saturating_add(text_bytes / TEXT_BYTES_PER_NODE);
        match self.remaining.checked_sub(units) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(())
            }
            None => Err(anyhow::anyhow!(
                "resource_limit: inherited_nodes: complexContent extensions copy more than \
                 {MAX_INHERITED_NODES} particles, attributes and enumeration values"
            )),
        }
    }
}

/// Bytes of copied text charged as one node.
///
/// Names, enumeration values and patterns are copied with the node that holds
/// them and can be as long as the schema, so their bytes are charged too. A
/// node that carries 30 to 50 bytes of names and type references is charged
/// 10 to 20 percent more than a node alone.
const TEXT_BYTES_PER_NODE: usize = 256;

/// What copying some inherited content costs the [`InheritanceBudget`].
#[derive(Debug, Clone, Copy, Default)]
struct CopyCost {
    /// Separately allocated values: particles, attributes, enumeration values
    /// and simple-content types.
    nodes: usize,
    /// Bytes of text owned by those values.
    text_bytes: usize,
}

impl CopyCost {
    /// Cost of copying `particles`, nested inline types included.
    ///
    /// The recursion follows the nesting of the model.
    fn of_particles(particles: &[Particle]) -> Self {
        let mut cost = Self::default();
        cost.add_particles(particles);
        cost
    }

    /// Cost of copying `attributes`.
    fn of_attributes(attributes: &[AttributeDecl]) -> Self {
        let mut cost = Self::default();
        cost.add_attributes(attributes);
        cost
    }

    fn add_node(&mut self) {
        self.nodes = self.nodes.saturating_add(1);
    }

    fn add_text(&mut self, text: &str) {
        self.text_bytes = self.text_bytes.saturating_add(text.len());
    }

    fn add_optional_text(&mut self, text: Option<&String>) {
        if let Some(text) = text {
            self.add_text(text);
        }
    }

    fn add_qname(&mut self, name: &QName) {
        self.add_optional_text(name.namespace.as_ref());
        self.add_text(&name.local);
    }

    /// A simple type costs its text and one node per enumeration value; the
    /// node that holds the type is charged by the caller.
    fn add_simple_type(&mut self, simple: &SimpleTypeDef) {
        if let Some(name) = &simple.name {
            self.add_qname(name);
        }
        if let SimpleTypeKind::Other(kind) = &simple.kind {
            self.add_text(kind);
        }
        let facets = &simple.facets;
        for value in &facets.enumerations {
            self.add_node();
            self.add_text(&value.value);
            self.add_optional_text(value.documentation.as_ref());
        }
        for text in [
            &facets.pattern,
            &facets.min_inclusive,
            &facets.max_inclusive,
            &facets.min_exclusive,
            &facets.max_exclusive,
        ] {
            self.add_optional_text(text.as_ref());
        }
    }

    fn add_attributes(&mut self, attributes: &[AttributeDecl]) {
        for attribute in attributes {
            self.add_node();
            self.add_text(&attribute.name);
            self.add_optional_text(attribute.default.as_ref());
            self.add_optional_text(attribute.fixed.as_ref());
            self.add_simple_type(&attribute.type_def);
        }
    }

    fn add_particles(&mut self, particles: &[Particle]) {
        for particle in particles {
            self.add_node();
            match &particle.element {
                ElementRef::Inline(decl) => {
                    self.add_qname(&decl.name);
                    self.add_optional_text(decl.documentation.as_ref());
                    match &decl.type_def {
                        XsdType::Simple(simple) => self.add_simple_type(simple),
                        XsdType::Complex(inner) => self.add_complex_type(inner),
                        XsdType::Ref(name) => self.add_qname(name),
                    }
                }
                ElementRef::Group(group) => self.add_content(group),
                ElementRef::Ref(name) => self.add_qname(name),
            }
        }
    }

    fn add_complex_type(&mut self, complex: &ComplexType) {
        for name in complex.name.iter().chain(complex.base.iter()) {
            self.add_qname(name);
        }
        for name in &complex.prohibited_attributes {
            self.add_text(name);
        }
        self.add_attributes(&complex.attributes);
        self.add_content(&complex.content);
    }

    fn add_content(&mut self, content: &ContentModel) {
        match content {
            ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => {
                self.add_particles(p);
            }
            ContentModel::SimpleContent(simple) => {
                self.add_node();
                self.add_simple_type(simple);
            }
            ContentModel::Empty => {}
        }
    }
}

/// Deepest nesting of inline types and model groups folded in one type.
///
/// A backstop, not the rule that ends a fold. Folding recurses once per
/// level, and a fold can nest deeper than the schema text does: every base
/// copied into an inline type brings its own inline types with it. A
/// recursive type does not reach this: a base is not folded again inside
/// its own copy (see `resolve_complex_type`). What is left to reach it is a
/// long run of different named types, each with an inline child extending
/// the next, or a model built or deserialized by a caller; such a model is
/// refused instead of being left partly folded.
const MAX_INHERITANCE_NESTING: usize = 256;

/// Resolve one complex type in place: fold its base, then recurse into inline
/// children so nested `complexContent`/`extension` is resolved as well.
///
/// `owner` is the type's own name (for named types), used to reject a base that
/// points straight back at the type itself. `depth` is the nesting of this
/// type inside the named type being resolved; it has no bearing on how long
/// a chain of bases is followed.
///
/// `folding` names the bases folded into the inline types around this one:
/// every named type whose copied particles the walk is inside of. A named
/// type can hold an element whose inline type extends that same type,
/// directly or through other types. That is legal, and folding it has no
/// end by itself, because the copied base holds the inline extension again.
/// So an inline type whose base is already in `folding` is not folded: it
/// keeps the particles and attributes it declares itself, and its `base`.
/// The recursion is thereby unrolled exactly once. The bases of a type that
/// is not nested (`depth` 0) are its own content and are not named: the
/// first inline extension inside them is still folded, whether the type is
/// the recursive one or derives from it.
fn resolve_complex_type<'a>(
    ct: &mut ComplexType,
    owner: Option<&str>,
    originals: &'a IndexMap<String, ComplexType>,
    depth: usize,
    folding: &mut Vec<&'a str>,
    budget: &mut InheritanceBudget,
) -> anyhow::Result<()> {
    check_inheritance_nesting(depth)?;
    let outer = folding.len();
    // How many of the type's particles are copies of its bases'.
    let mut copied = 0;
    // Fold the base chain into this type's content + attributes.
    let base = ct
        .base
        .as_ref()
        .filter(|base| !folding.contains(&base.local.as_str()));
    if let Some(base) = base.cloned() {
        if let Some((mut base_particles, mut base_attrs)) =
            collect_base(&base.local, owner, originals, folding, budget)?
        {
            if ct.derivation == Derivation::Restriction {
                // A restriction states its particles itself; only the
                // attributes come from the base.
                restrict_attributes(
                    &mut base_attrs,
                    std::mem::take(&mut ct.attributes),
                    &ct.prohibited_attributes,
                );
            } else {
                match &mut ct.content {
                    // The type's own particles move behind the inherited ones.
                    ContentModel::Sequence(own) => {
                        copied = base_particles.len();
                        base_particles.append(own);
                        *own = base_particles;
                    }
                    // Pure extension that only adds attributes: keep the base's
                    // particles verbatim.
                    ContentModel::Empty => {
                        copied = base_particles.len();
                        ct.content = ContentModel::Sequence(base_particles);
                    }
                    ContentModel::All(_)
                    | ContentModel::Choice(_)
                    | ContentModel::SimpleContent(_) => {}
                }
                // Inherited attributes come first, then locally declared ones.
                base_attrs.append(&mut ct.attributes);
            }
            ct.attributes = base_attrs;
        }
    }

    // Recurse into inline complex types so nested extensions are resolved too.
    if depth == 0 {
        folding.truncate(outer);
    }
    // The bases folded into a nested type stay named while their copies
    // are walked, and only then: the particles the type declares itself are
    // not a copy of anything, so an extension of the same base among them is
    // not recursion.
    if let Some(particles) = content_particles_mut(&mut ct.content) {
        for (position, particle) in particles.iter_mut().enumerate() {
            if position == copied {
                folding.truncate(outer);
            }
            resolve_element_ref(&mut particle.element, originals, depth + 1, folding, budget)?;
        }
    }
    folding.truncate(outer);
    Ok(())
}

/// Refuses a model nested deeper than [`MAX_INHERITANCE_NESTING`].
fn check_inheritance_nesting(depth: usize) -> anyhow::Result<()> {
    if depth > MAX_INHERITANCE_NESTING {
        anyhow::bail!(
            "resource_limit: inheritance_nesting_depth: content models nest deeper than \
             {MAX_INHERITANCE_NESTING}"
        );
    }
    Ok(())
}

/// Resolve any inline complex type reachable through one particle's element.
fn resolve_element_ref<'a>(
    element: &mut ElementRef,
    originals: &'a IndexMap<String, ComplexType>,
    depth: usize,
    folding: &mut Vec<&'a str>,
    budget: &mut InheritanceBudget,
) -> anyhow::Result<()> {
    check_inheritance_nesting(depth)?;
    match element {
        ElementRef::Inline(decl) => {
            if let XsdType::Complex(inner) = &mut decl.type_def {
                // Inline types are anonymous; they have no name to self-reference.
                resolve_complex_type(inner, None, originals, depth, folding, budget)?;
            }
        }
        ElementRef::Group(group) => {
            if let Some(particles) = content_particles_mut(group) {
                for particle in particles {
                    resolve_element_ref(
                        &mut particle.element,
                        originals,
                        depth + 1,
                        folding,
                        budget,
                    )?;
                }
            }
        }
        ElementRef::Ref(_) => {}
    }
    Ok(())
}

/// One base's effective particles and attributes, ancestors first.
type Inherited = (Vec<Particle>, Vec<AttributeDecl>);

/// Collect a base type's effective particles and attributes, oldest ancestor
/// first, following the whole chain of bases. Returns `None` if the base is
/// unknown or is `owner`, the type being resolved.
///
/// The chain ends at a type with no base, at a base that is not a known
/// complex type, or at a name already met (a cycle, which is broken there).
/// It is as long as the schema makes it: nothing is left out for being far
/// from the derived type. Each type on the chain costs `budget` one unit, so
/// that the walk itself is bounded, and everything copied is charged at its
/// [`CopyCost`] before the copy is made. A chain the budget cannot pay for is
/// an error, never a partly inherited type.
///
/// The name of every type on the chain is appended to `folding`.
fn collect_base<'a>(
    base_local: &str,
    owner: Option<&str>,
    originals: &'a IndexMap<String, ComplexType>,
    folding: &mut Vec<&'a str>,
    budget: &mut InheritanceBudget,
) -> anyhow::Result<Option<Inherited>> {
    // Nearest base first.
    let mut chain: Vec<&ComplexType> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = owner.into_iter().collect();
    let mut next = Some(base_local);
    while let Some(name) = next {
        if !seen.insert(name) {
            break;
        }
        let Some((name, base)) = originals.get_key_value(name) else {
            break;
        };
        // `seen` holds the map's own key from here on, which outlives `next`.
        seen.insert(name.as_str());
        budget.charge(CopyCost {
            nodes: 1,
            text_bytes: 0,
        })?;
        chain.push(base);
        folding.push(name.as_str());
        next = base.base.as_ref().map(|base| base.local.as_str());
    }
    if chain.is_empty() {
        return Ok(None);
    }

    let mut particles: Vec<Particle> = Vec::new();
    let mut attrs: Vec<AttributeDecl> = Vec::new();
    for base in chain.into_iter().rev() {
        let restricts = base.base.is_some() && base.derivation == Derivation::Restriction;
        if restricts {
            // A restriction on the chain replaces the particles of
            // everything before it with the ones it states.
            particles.clear();
        }
        if let ContentModel::Sequence(p) = &base.content {
            budget.charge(CopyCost::of_particles(p))?;
            particles.extend(p.iter().cloned());
        }
        budget.charge(CopyCost::of_attributes(&base.attributes))?;
        if restricts {
            restrict_attributes(
                &mut attrs,
                base.attributes.clone(),
                &base.prohibited_attributes,
            );
        } else {
            attrs.extend(base.attributes.iter().cloned());
        }
    }
    Ok(Some((particles, attrs)))
}

/// Applies a restriction's attribute declarations to the attributes it
/// inherits.
///
/// An inherited attribute named in `prohibited` is removed. One the
/// restriction declares again stays where it is and keeps the type the base
/// gave it, with the restriction's `use` and whichever default and fixed
/// value the restriction states. An attribute the base does not have is
/// appended.
fn restrict_attributes(
    inherited: &mut Vec<AttributeDecl>,
    own: Vec<AttributeDecl>,
    prohibited: &[String],
) {
    if !prohibited.is_empty() {
        let prohibited: std::collections::HashSet<&str> =
            prohibited.iter().map(String::as_str).collect();
        inherited.retain(|attribute| !prohibited.contains(attribute.name.as_str()));
    }
    let positions: std::collections::HashMap<String, usize> = inherited
        .iter()
        .enumerate()
        .map(|(position, attribute)| (attribute.name.clone(), position))
        .collect();
    for attribute in own {
        match positions
            .get(&attribute.name)
            .and_then(|position| inherited.get_mut(*position))
        {
            Some(existing) => {
                existing.required = attribute.required;
                if attribute.default.is_some() {
                    existing.default = attribute.default;
                }
                if attribute.fixed.is_some() {
                    existing.fixed = attribute.fixed;
                }
            }
            None => inherited.push(attribute),
        }
    }
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
