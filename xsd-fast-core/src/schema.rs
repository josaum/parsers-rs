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
    ///
    /// [`SimpleTypeKind::String`] for a list. For a union, the kind every
    /// member has, or [`SimpleTypeKind::String`] when the members differ.
    pub kind: SimpleTypeKind,
    /// Supported restriction facets.
    ///
    /// On a list, `min_length` and `max_length` count items, as XML Schema
    /// measures the length of a list.
    pub facets: SimpleFacets,
    /// The type of each item when this is an `xs:list`, or a restriction of
    /// one.
    ///
    /// `None` for every other type, and for a model serialized before the
    /// field existed. Left out of the serialized form when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_item: Option<Box<SimpleTypeDef>>,
    /// The member types when this is an `xs:union`, or a restriction of one,
    /// in the order they are written: `memberTypes` first, then the inline
    /// members.
    ///
    /// Empty for every other type, and for a model serialized before the
    /// field existed. Left out of the serialized form when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub union_members: Vec<SimpleTypeDef>,
}

impl SimpleTypeDef {
    /// A simple type of `kind` with no name, facets, items or members.
    pub(crate) fn of_kind(kind: SimpleTypeKind) -> Self {
        Self {
            name: None,
            kind,
            facets: SimpleFacets::default(),
            list_item: None,
            union_members: Vec::new(),
        }
    }

    pub(crate) fn string_unbounded() -> Self {
        Self::of_kind(SimpleTypeKind::String)
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
    /// Base type of an extension or restriction against a *complex* base,
    /// under `complexContent` or, for a restriction, `simpleContent`.
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
    /// Whether the type was written with nothing in it but annotations:
    /// `<xs:complexType/>`, or the open-and-closed form of that.
    ///
    /// This is not the same as having no particles and no attributes. The
    /// parser leaves out what it does not model (attribute references,
    /// attribute groups, group references, wildcards, a derivation from a
    /// base it cannot find), so a type built only from those has neither
    /// and is not empty. `false` for a model that was deserialized without
    /// this field or built by a caller that does not set it.
    #[serde(default)]
    pub empty_in_source: bool,
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
    /// Whether the attribute reached its type through `xs:attribute ref` or
    /// `xs:attributeGroup ref`, which earlier versions did not resolve.
    ///
    /// Arrow derivation leaves such an attribute out of a child element
    /// whose other content derives exactly one column, so that element keeps
    /// the shape it had before references were resolved. `false` for a
    /// model serialized before the field existed, and left out of the
    /// serialized form when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub referenced: bool,
}

/// Whether `value` is `false`; decides when [`AttributeDecl::referenced`]
/// is left out of the serialized form.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
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
    /// out, and one it declares again keeps the base's type. A restriction
    /// of a base with simple content also takes the type of its text. Inline complex types nested in particles, and
    /// the inline types of global elements, are resolved too, so inheritance
    /// works regardless of where it appears. A chain of bases is followed to
    /// its end, however long. Types that do not inherit are left
    /// byte-identical.
    ///
    /// A recursive type, one holding an element whose inline type extends
    /// the type itself or a type that leads back to it, is unrolled to a
    /// fixed nesting, and less far when the copies that takes are more than
    /// one type is allowed. Where the unrolling ends, and below the deepest
    /// nesting that is folded at all, an inline type keeps only what it
    /// declares itself. Neither is an error.
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
        let mut budget = FoldBudget {
            required: InheritanceBudget {
                remaining: MAX_INHERITED_NODES,
                uncharged_text_bytes: 0,
            },
            optional: InheritanceBudget {
                remaining: MAX_OPTIONAL_NODES,
                uncharged_text_bytes: 0,
            },
            allowance: 0,
        };

        let mut folding = Vec::new();

        for (name, ct) in self.complex_types.iter_mut() {
            budget.allowance = MAX_OPTIONAL_NODES_PER_TYPE;
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
                budget.allowance = MAX_OPTIONAL_NODES_PER_TYPE;
                resolve_complex_type(inline, None, &originals, 0, &mut folding, &mut budget)?;
            }
        }
        Ok(())
    }
}

/// Most that the folds a schema is wrong without may copy in one schema
/// set, in budget units.
///
/// A unit is one copied particle, attribute, enumeration value or
/// simple-content type, or [`TEXT_BYTES_PER_NODE`] bytes of the text they
/// own; see [`CopyCost`].
///
/// Folding is eager: every derived type receives its own copy of the
/// content of each of its bases, and an inline type that extends a named
/// type receives a copy that can itself hold inline extensions. A schema of
/// a few hundred kilobytes can therefore ask for far more than it holds:
/// two thousand types that each extend one base of a hundred elements copy
/// two hundred thousand particles, and a base holding one large enumeration
/// is copied whole into every type derived from it.
///
/// This budget pays for the folds that were always made: the nearest
/// [`REQUIRED_CHAIN_LINKS`] bases of a type, at a nesting below
/// [`REQUIRED_FOLD_DEPTH`], recursion apart. A schema set that asks for
/// more of those than the budget holds is refused with a `resource_limit`
/// error. Everything else folding does (unrolling a recursive type, deeper
/// nesting, bases further up a chain) is paid from [`MAX_OPTIONAL_NODES`]
/// and never refuses a schema.
///
/// The limit is a bound on memory. A schema set that spends all of this
/// budget and none of the other peaks at about 130 MiB of process memory
/// when the copies are plain element particles (132 MiB measured), about
/// 120 MiB for inline types with attributes, and about 70 MiB for long
/// names or values (on a 64-bit build, loading included). One that spends
/// both budgets peaks at about 200 MiB for plain element particles (196 MiB
/// measured, of which the schema itself is 10 MiB); that is the size to plan
/// for, since the two budgets are spent independently. A schema that is
/// refused can peak higher while it is being read: the largest seen is
/// 332 MiB, for a 4.6 MiB schema file. Changing what a unit charges needs
/// these measurements repeated.
const MAX_INHERITED_NODES: usize = 1 << 18;

/// Most that the folds a schema can do without may copy in one schema set,
/// in the units of [`MAX_INHERITED_NODES`].
///
/// A fold is one a schema can do without when it unrolls a recursive type,
/// lies at [`REQUIRED_FOLD_DEPTH`] or deeper, or brings in a base further
/// up a chain than [`REQUIRED_CHAIN_LINKS`]. Such folds are paid from this
/// pool and never from the one that required folds use, so they cannot
/// make a required fold fail; when the pool is empty, or the type being
/// resolved has used its [`MAX_OPTIONAL_NODES_PER_TYPE`], they are left out
/// without an error. Types are resolved in the order they are declared, so
/// the pool goes to the first types that ask for it.
///
/// It bounds memory as [`MAX_INHERITED_NODES`] does and is the same size:
/// a folded model holds at most the copies of both, 2^19 units, on top of
/// the schema as written. The measured peaks are given there.
const MAX_OPTIONAL_NODES: usize = MAX_INHERITED_NODES;

/// Most that the folds a schema can do without may copy inside one named
/// type or one global element, in the same units.
///
/// It keeps one type from emptying [`MAX_OPTIONAL_NODES`] for all the
/// others. A type with two recursive children doubles its copies with every
/// level, and is stopped here after 2^14 units (about 8 MiB) instead of the
/// 2^17 copies sixteen levels ask for; a type with one recursive child of a
/// thousand nodes is still unrolled to the full depth. Once a type has no
/// allowance left none of its remaining folds of this kind are made.
const MAX_OPTIONAL_NODES_PER_TYPE: usize = 1 << 14;

/// Nesting from which a fold is one the schema can do without, and at which
/// a recursive type is no longer unrolled.
///
/// A named type can hold an element whose inline type extends that same
/// type, directly or through other types. That is legal, and folding it has
/// no end by itself: the copied base holds the inline extension again.
/// Sixteen is the nesting at which folding used to stop altogether, so a
/// recursive type is unrolled as deep as it always was, and what is folded
/// below this nesting is exactly what was folded then. Raising it deepens
/// every later walk over the model (validation, Arrow derivation, `Clone`,
/// `Drop`).
const REQUIRED_FOLD_DEPTH: usize = 16;

/// How many of a type's nearest bases are always folded into it.
///
/// A chain of bases used to be followed this far and no further. These
/// links are paid from the required budget, so their cost can refuse a
/// schema, exactly as it always could. Bases further away are a fold the
/// schema can do without: they are folded while [`MAX_OPTIONAL_NODES`] and
/// [`MAX_OPTIONAL_NODES_PER_TYPE`] last, and left out after that.
const REQUIRED_CHAIN_LINKS: usize = 16;

/// The two budgets inheritance folding draws on.
struct FoldBudget {
    /// For the folds a schema is wrong without; running out is an error.
    required: InheritanceBudget,
    /// For the folds it can do without; see [`MAX_OPTIONAL_NODES`].
    optional: InheritanceBudget,
    /// What the type now being resolved may still take from `optional`;
    /// see [`MAX_OPTIONAL_NODES_PER_TYPE`].
    allowance: usize,
}

impl FoldBudget {
    /// Charges `cost` to the budget for folds a schema can do without.
    ///
    /// Returns `false`, with nothing spent, when the pool or the allowance
    /// of the type being resolved cannot pay for it; the type then has no
    /// allowance left.
    fn charge_optional(&mut self, cost: CopyCost) -> bool {
        let available = self.optional.remaining.min(self.allowance);
        let mut budget = InheritanceBudget {
            remaining: available,
            uncharged_text_bytes: self.optional.uncharged_text_bytes,
        };
        if budget.charge(cost).is_err() {
            self.allowance = 0;
            return false;
        }
        let spent = available - budget.remaining;
        self.optional.remaining -= spent;
        self.optional.uncharged_text_bytes = budget.uncharged_text_bytes;
        self.allowance -= spent;
        true
    }
}

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
/// Folding recurses once per level, and a fold can nest deeper than the
/// schema text does: every base copied into an inline type brings its own
/// inline types with it. A long run of different named types, each with an
/// inline child extending the next, nests as deep as the run is long. What
/// lies deeper than this is left as written, without an error: its inline
/// types keep the particles and attributes they declare themselves.
const MAX_INHERITANCE_NESTING: usize = 256;

/// Resolve one complex type in place: fold its base, then recurse into inline
/// children so nested `complexContent`/`extension` is resolved as well.
///
/// `owner` is the type's own name (for named types), used to reject a base that
/// points straight back at the type itself. `depth` is the nesting of this
/// type inside the named type being resolved; it has no bearing on how long
/// a chain of bases is followed.
///
/// `folding` names the bases whose copied particles the walk is inside of.
/// A base met again there is recursion: its fold is one the schema can do
/// without, and is not made at [`REQUIRED_FOLD_DEPTH`] or deeper. A fold
/// that is not made, for that reason or because its budget is spent, is
/// left out without an error: the type keeps the particles and attributes
/// it declares itself, and its `base`.
fn resolve_complex_type<'a>(
    ct: &mut ComplexType,
    owner: Option<&str>,
    originals: &'a IndexMap<String, ComplexType>,
    depth: usize,
    folding: &mut Vec<&'a str>,
    budget: &mut FoldBudget,
) -> anyhow::Result<()> {
    if depth > MAX_INHERITANCE_NESTING {
        return Ok(());
    }
    let outer = folding.len();
    // How many of the type's particles are copies of its bases'.
    let mut copied = 0;
    // Fold the base chain into this type's content + attributes.
    if let Some(base) = ct.base.clone() {
        let recursive = folding.contains(&base.local.as_str());
        let inherited = if depth < REQUIRED_FOLD_DEPTH && !recursive {
            collect_base(
                &base.local,
                owner,
                originals,
                folding,
                budget,
                REQUIRED_CHAIN_LINKS,
            )?
        } else if depth < REQUIRED_FOLD_DEPTH || !recursive {
            // Nothing of this fold is required, so it cannot fail.
            collect_base(&base.local, owner, originals, folding, budget, 0)?
        } else {
            None
        };
        if let Some((mut base_particles, mut base_attrs, base_text)) = inherited {
            if ct.derivation == Derivation::Restriction {
                // A restriction states its particles itself; only the
                // attributes come from the base, and the type of the text
                // when the base has simple content and the restriction
                // states no content of its own.
                restrict_attributes(
                    &mut base_attrs,
                    std::mem::take(&mut ct.attributes),
                    &ct.prohibited_attributes,
                );
                if let (ContentModel::Empty, Some(text)) = (&ct.content, base_text) {
                    ct.content = ContentModel::SimpleContent(text);
                }
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
                extend_attributes(&mut base_attrs, std::mem::take(&mut ct.attributes));
            }
            ct.attributes = base_attrs;
        }
    }

    // Recurse into inline complex types so nested extensions are resolved too.
    // The bases just folded stay named while their copies are walked, and
    // only then: the particles the type declares itself are
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

/// Resolve any inline complex type reachable through one particle's element.
fn resolve_element_ref<'a>(
    element: &mut ElementRef,
    originals: &'a IndexMap<String, ComplexType>,
    depth: usize,
    folding: &mut Vec<&'a str>,
    budget: &mut FoldBudget,
) -> anyhow::Result<()> {
    if depth > MAX_INHERITANCE_NESTING {
        return Ok(());
    }
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

/// One base's effective particles and attributes, ancestors first, and the
/// type of its text when the base has simple content.
type Inherited = (
    Vec<Particle>,
    Vec<AttributeDecl>,
    Option<Box<SimpleTypeDef>>,
);

/// Collect a base type's effective particles and attributes, oldest ancestor
/// first, following the chain of bases. Returns `None` if the base is
/// unknown or is `owner`, the type being resolved, or if nothing of the
/// chain can be paid for.
///
/// The chain ends at a type with no base, at a base that is not a known
/// complex type, or at a name already met (a cycle, which is broken there).
/// Each type on it costs one unit and the [`CopyCost`] of what is copied
/// from it, charged before anything is copied, nearest base first.
///
/// The nearest `required_links` types are paid from the required budget,
/// and a chain that budget cannot pay for is an error. The types beyond
/// them are paid from the budget for folds a schema can do without; the
/// chain is cut, without an error, at the first one that budget cannot pay
/// for, and the derived type then lacks what its furthest ancestors
/// declare. The walk stops there too, so its length is bounded by what was
/// paid.
///
/// The name of every type kept on the chain is appended to `folding`.
fn collect_base<'a>(
    base_local: &str,
    owner: Option<&str>,
    originals: &'a IndexMap<String, ComplexType>,
    folding: &mut Vec<&'a str>,
    budget: &mut FoldBudget,
    required_links: usize,
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
        let mut cost = CopyCost::of_attributes(&base.attributes);
        cost.add_node();
        if let ContentModel::Sequence(p) = &base.content {
            cost.add_particles(p);
        }
        if chain.len() < required_links {
            budget.required.charge(cost)?;
        } else if !budget.charge_optional(cost) {
            break;
        }
        chain.push(base);
        folding.push(name.as_str());
        next = base.base.as_ref().map(|base| base.local.as_str());
    }
    if chain.is_empty() {
        return Ok(None);
    }

    let mut particles: Vec<Particle> = Vec::new();
    let mut attrs: Vec<AttributeDecl> = Vec::new();
    // The simple content nearest to the derived type.
    let mut text: Option<&SimpleTypeDef> = None;
    for base in chain.into_iter().rev() {
        if let ContentModel::SimpleContent(simple) = &base.content {
            text = Some(simple);
        }
        let restricts = base.base.is_some() && base.derivation == Derivation::Restriction;
        if restricts {
            // A restriction on the chain replaces the particles of
            // everything before it with the ones it states.
            particles.clear();
        }
        if let ContentModel::Sequence(p) = &base.content {
            particles.extend(p.iter().cloned());
        }
        if restricts {
            restrict_attributes(
                &mut attrs,
                base.attributes.clone(),
                &base.prohibited_attributes,
            );
        } else {
            extend_attributes(&mut attrs, base.attributes.clone());
        }
    }
    let text = match text {
        Some(simple) => {
            let mut cost = CopyCost::default();
            cost.add_node();
            cost.add_simple_type(simple);
            let paid = if required_links > 0 {
                budget.required.charge(cost)?;
                true
            } else {
                budget.charge_optional(cost)
            };
            paid.then(|| Box::new(simple.clone()))
        }
        None => None,
    };
    Ok(Some((particles, attrs, text)))
}

/// Appends an extension's attribute declarations to the attributes it
/// inherits, one attribute per name.
///
/// A declaration whose name is already inherited replaces the inherited one
/// where it stands: the derived type's own declaration wins. Any other is
/// appended.
fn extend_attributes(inherited: &mut Vec<AttributeDecl>, own: Vec<AttributeDecl>) {
    if own.is_empty() {
        return;
    }
    let mut positions: std::collections::HashMap<String, usize> = inherited
        .iter()
        .enumerate()
        .map(|(position, attribute)| (attribute.name.clone(), position))
        .collect();
    for attribute in own {
        match positions
            .get(&attribute.name)
            .and_then(|position| inherited.get_mut(*position))
        {
            Some(existing) => *existing = attribute,
            None => {
                positions.insert(attribute.name.clone(), inherited.len());
                inherited.push(attribute);
            }
        }
    }
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
