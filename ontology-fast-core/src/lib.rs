//! Canonical Turtle reader and indexed triple view for RDF ontology routing.
//!
//! This crate parses Turtle documents into a canonical text-triple view and an
//! indexed `(subject, predicate, object)` lookup table.
//!
//! Text form, derived from the document's own prefix declarations (`@prefix`
//! or SPARQL-style `PREFIX`, as the Turtle parser reports them — see
//! [`declared_prefixes`]):
//!
//! * a prefixed name stays a prefixed name (`ex:ROAS`)
//! * `rdf:type` renders as `a`
//! * a literal renders as its lexical form, without quotes
//! * an IRI no declared prefix covers stays `<...>`, never a guess
//!
//! [`parse_turtle_terms`] is the lossless companion view for app surfaces that
//! must rebuild their current graph term for term: full IRIs, literal
//! datatypes, and language tags are preserved rather than erased.
//!
//! # Refusals
//!
//! Rendering is not attempted for terms whose text form would be an artefact of
//! this implementation rather than the document:
//!
//! * RDF 1.2 triple terms — the routing index has no term for them
//! * named graphs — this reader models one graph
//! * RDF 1.2 base-direction literals — the term view has no direction to carry
//!
//! Blank nodes read in full: the label is document-local (RDF 1.2 Concepts
//! §3.5) and carries no meaning, so every blank node is relabelled
//! `_:d<scope>b<n>` — scoped to the source bytes, numbered by first
//! appearance. The same bytes always read to the same text, two documents
//! never share a label, and comparison with another implementation is still
//! left to isomorphism. The refusals fail closed with [`OntologyError`].
//! Nothing here reads a source system, signs data, or promotes a fact: parsing
//! preserves structure and provenance, and the output is unsigned.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use oxrdf::{GraphName, NamedOrBlankNode, Quad, Term};
use oxrdfio::{RdfFormat, RdfParser};
use thiserror::Error;

/// The RDF type IRI, rendered as Turtle's own `a` keyword.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The XSD `string` datatype IRI (XML Schema Part 2 §3.2.1).
///
/// RDF 1.2 Concepts §3.3 makes a simple literal and `"..."^^xsd:string` the
/// same term, and `oxrdfio` reports both with this datatype. The term view
/// maps it back to "no datatype", which is how the app's previous reader
/// (rdflib) reports a simple literal. A source that writes `^^xsd:string`
/// explicitly therefore reads as a simple literal here while rdflib keeps the
/// datatype: a recorded contract difference, not a parse failure.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Fail-closed resource budget for one parse.
#[derive(Clone, Copy, Debug)]
pub struct OntologyLimits {
    /// Maximum source bytes accepted.
    pub max_source_bytes: usize,
    /// Maximum triples retained.
    pub max_statements: usize,
}

impl OntologyLimits {
    /// Return the default budget with each given ceiling replaced.
    ///
    /// This is the keyword-argument shape both language bindings expose: a
    /// caller tightens (or widens) one ceiling and inherits the rest.
    #[must_use]
    pub fn with_overrides(max_source_bytes: Option<usize>, max_statements: Option<usize>) -> Self {
        let defaults = Self::default();
        Self {
            max_source_bytes: max_source_bytes.unwrap_or(defaults.max_source_bytes),
            max_statements: max_statements.unwrap_or(defaults.max_statements),
        }
    }
}

impl Default for OntologyLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024 * 1024,
            max_statements: 2_000_000,
        }
    }
}

/// One triple in the routing core's text form.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TripleText {
    /// Subject, abbreviated when a declared prefix covers it.
    pub subject: String,
    /// Predicate, `a` for `rdf:type`.
    pub predicate: String,
    /// Object: a literal's lexical form, or an abbreviated IRI.
    pub object: String,
}

impl TripleText {
    /// Tab-separated line form used across the Python boundary.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!("{}\t{}\t{}", self.subject, self.predicate, self.object)
    }
}

/// One RDF term with the structure the routing text contract erases.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TermView {
    /// A full IRI; never abbreviated.
    Iri(String),
    /// A blank node as `_:d<scope>b<n>`: the same for every parse of the same bytes.
    Blank(String),
    /// A literal with its datatype or language tag preserved.
    Literal {
        /// The lexical form.
        value: String,
        /// The datatype IRI of a typed literal; `None` for simple and language-tagged literals.
        datatype: Option<String>,
        /// The language tag of a language-tagged literal, lowercased; otherwise `None`.
        language: Option<String>,
    },
}

/// One triple in the lossless term view: full IRIs, literal typing preserved.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TripleTerms {
    /// The subject: a full IRI, or a document-scoped `_:label` blank node.
    ///
    /// A subject is never a literal in RDF 1.2, and no IRI carries the `_`
    /// scheme, so the `_:label` prefix distinguishes the two cases in one string.
    pub subject: String,
    /// The predicate IRI; never abbreviated, and `rdf:type` stays a full IRI.
    pub predicate: String,
    /// The object term.
    pub object: TermView,
}

/// Typed refusal. Never a silent drop.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OntologyError {
    /// The document could not be parsed as Turtle.
    #[error("ontology syntax: {0}")]
    Syntax(String),
    /// A resource budget was exceeded.
    #[error("ontology resource limit: {resource} limit={limit} observed={observed}")]
    ResourceLimit {
        /// Which budget was exceeded.
        resource: &'static str,
        /// The configured ceiling.
        limit: usize,
        /// What the document actually needed.
        observed: usize,
    },
    /// An RDF 1.2 triple term reached rendering.
    #[error("RDF 1.2 triple term in {position} is not representable")]
    TripleTerm {
        /// `subject` or `object`.
        position: &'static str,
    },
    /// An RDF 1.2 base-direction literal reached the lossless term view.
    ///
    /// The view — like the app's previous reader, rdflib — has no direction to
    /// carry, so the term is refused rather than rendered without one.
    #[error("RDF 1.2 base direction on a literal in {position} is not representable")]
    BaseDirection {
        /// `subject` or `object`.
        position: &'static str,
    },
    /// A quad carried a named graph; this reader models one graph.
    #[error("named graph {0} is not supported by the triple reader")]
    NamedGraph(String),
}

impl OntologyError {
    /// Stable machine-readable refusal code.
    ///
    /// Bindings map refusals to exception types through this code, so a
    /// reworded message never changes what a caller can catch.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Syntax(_) => "syntax",
            Self::ResourceLimit { .. } => "resource_limit",
            Self::TripleTerm { .. } => "triple_term",
            Self::BaseDirection { .. } => "base_direction",
            Self::NamedGraph(_) => "named_graph",
        }
    }
}

/// A parsed ontology: the triple text set, the declared prefixes, and an index.
#[derive(Clone, Debug)]
pub struct ParsedOntology {
    triples: Vec<TripleText>,
    prefixes: BTreeMap<String, String>,
    index: OntologyIndex,
}

impl ParsedOntology {
    /// Triples in source order, deduplicated.
    #[must_use]
    pub fn triples(&self) -> &[TripleText] {
        &self.triples
    }

    /// Prefixes the document itself declared.
    #[must_use]
    pub fn prefixes(&self) -> &BTreeMap<String, String> {
        &self.prefixes
    }

    /// The indexed view, for `(s, ?, ?)` style walks the Python core performs.
    #[must_use]
    pub fn index(&self) -> &OntologyIndex {
        &self.index
    }
}

/// Subject/predicate/object lookup over one parsed ontology.
///
/// The Python core currently answers its route and entity queries by iterating
/// every triple for every pattern. This is the same answer without the scan.
#[derive(Clone, Debug, Default)]
pub struct OntologyIndex {
    by_subject: BTreeMap<String, Vec<u32>>,
    by_predicate: BTreeMap<String, Vec<u32>>,
    by_object: BTreeMap<String, Vec<u32>>,
}

impl OntologyIndex {
    /// Row ordinals whose subject is exactly `subject`.
    #[must_use]
    pub fn by_subject(&self, subject: &str) -> &[u32] {
        self.by_subject.get(subject).map_or(&[], Vec::as_slice)
    }

    /// Row ordinals whose predicate is exactly `predicate`.
    #[must_use]
    pub fn by_predicate(&self, predicate: &str) -> &[u32] {
        self.by_predicate.get(predicate).map_or(&[], Vec::as_slice)
    }

    /// Row ordinals whose object is exactly `object`.
    #[must_use]
    pub fn by_object(&self, object: &str) -> &[u32] {
        self.by_object.get(object).map_or(&[], Vec::as_slice)
    }

    /// Row ordinals matching an `(s, p, o)` pattern; `None` is a wildcard.
    ///
    /// The shortest bound column is walked and the others are probed by binary
    /// search, so the cost follows the most selective position rather than the
    /// order the positions happen to be written in. Ordinals come back
    /// ascending, which is source order. A pattern that binds nothing selects
    /// nothing: matching every row is a scan, not a lookup, and a caller that
    /// wants every triple already holds them.
    #[must_use]
    pub fn select(
        &self,
        subject: Option<&str>,
        predicate: Option<&str>,
        object: Option<&str>,
    ) -> Vec<u32> {
        let mut columns: Vec<&[u32]> = [
            subject.map(|value| self.by_subject(value)),
            predicate.map(|value| self.by_predicate(value)),
            object.map(|value| self.by_object(value)),
        ]
        .into_iter()
        .flatten()
        .collect();
        columns.sort_by_key(|column| column.len());

        let Some((shortest, rest)) = columns.split_first() else {
            return Vec::new();
        };
        shortest
            .iter()
            .copied()
            .filter(|ordinal| {
                // Every column is pushed in ascending ordinal order, which is
                // what makes the binary search valid.
                rest.iter()
                    .all(|column| column.binary_search(ordinal).is_ok())
            })
            .collect()
    }

    fn insert(&mut self, ordinal: u32, triple: &TripleText) {
        self.by_subject
            .entry(triple.subject.clone())
            .or_default()
            .push(ordinal);
        self.by_predicate
            .entry(triple.predicate.clone())
            .or_default()
            .push(ordinal);
        self.by_object
            .entry(triple.object.clone())
            .or_default()
            .push(ordinal);
    }
}

/// Parse Turtle bytes into the routing core's text form under explicit limits.
///
/// Terms are abbreviated only through prefixes the document declares, so a
/// cartridge can never depend on a prefix table baked into this reader. The
/// table is the one the Turtle parser itself ends the document with (see
/// [`declared_prefixes`]), so a directive-looking line inside a literal
/// declares nothing and a SPARQL-style `PREFIX` declares like `@prefix`.
///
/// # Errors
///
/// Returns [`OntologyError::Syntax`] for malformed Turtle or non-UTF-8 input,
/// [`OntologyError::ResourceLimit`] when a budget is exceeded,
/// [`OntologyError::NamedGraph`] when a quad names a graph, and
/// [`OntologyError::TripleTerm`] for an RDF 1.2 triple term. A base direction
/// is not refused here: the text form keeps a literal's lexical form only, so
/// direction is erased exactly as the language tag and datatype are.
pub fn parse_turtle(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<ParsedOntology, OntologyError> {
    // Rendering needs the document's final prefix table, which exists only
    // once the parser has reached the end, so full-IRI rows are staged first.
    // The stage is deduplicated and held to the statement budget, which keeps
    // retained memory bounded by the same ceiling as the result.
    let mut staged: Vec<StagedTriple> = Vec::new();
    let mut staged_seen: BTreeSet<StagedTriple> = BTreeSet::new();
    let prefixes = read_default_graph(source, limits, |quad, blank_labels| {
        // Subject before object: blank nodes are numbered by first appearance,
        // and both views must meet them in the same order.
        let subject = match &quad.subject {
            NamedOrBlankNode::NamedNode(node) => StagedTerm::Iri(node.as_str().to_owned()),
            NamedOrBlankNode::BlankNode(node) => {
                StagedTerm::Verbatim(blank_labels.label(node.as_str()))
            }
        };
        let object = match &quad.object {
            Term::NamedNode(node) => StagedTerm::Iri(node.as_str().to_owned()),
            Term::Literal(literal) => StagedTerm::Verbatim(literal.value().to_owned()),
            Term::BlankNode(node) => StagedTerm::Verbatim(blank_labels.label(node.as_str())),
            Term::Triple(_) => return Err(OntologyError::TripleTerm { position: "object" }),
        };
        let candidate = StagedTriple {
            subject,
            predicate: quad.predicate.as_str().to_owned(),
            object,
        };
        if staged_seen.insert(candidate.clone()) {
            enforce_statement_budget(staged_seen.len(), limits)?;
            staged.push(candidate);
        }
        Ok(())
    })?;
    drop(staged_seen);

    // Two staged rows can render to one text row (a literal whose lexical form
    // equals an abbreviated IRI), so the text set is deduplicated on its own.
    let mut triples: Vec<TripleText> = Vec::with_capacity(staged.len());
    let mut seen: BTreeSet<TripleText> = BTreeSet::new();
    for row in staged {
        let candidate = TripleText {
            subject: row.subject.render(&prefixes),
            predicate: predicate_text(&row.predicate, &prefixes),
            object: row.object.render(&prefixes),
        };
        if seen.insert(candidate.clone()) {
            triples.push(candidate);
        }
    }

    let mut index = OntologyIndex::default();
    for (ordinal, triple) in triples.iter().enumerate() {
        let ordinal = u32::try_from(ordinal).map_err(|_| OntologyError::ResourceLimit {
            resource: "index_ordinals",
            limit: u32::MAX as usize,
            observed: triples.len(),
        })?;
        index.insert(ordinal, triple);
    }

    Ok(ParsedOntology {
        triples,
        prefixes,
        index,
    })
}

/// Parse Turtle bytes into the lossless term view under explicit limits.
///
/// Where [`parse_turtle`] renders the routing core's text contract, this view
/// keeps what the contract erases — full IRIs and literal typing — so an app
/// surface can rebuild its current graph term for term instead of re-parsing
/// with a second reader. Triples are deduplicated and kept in source order,
/// exactly as in [`parse_turtle`]. Language tags are lowercased, matching the
/// normalization the app's previous reader (rdflib) applies on construction.
///
/// # Errors
///
/// Returns [`OntologyError::Syntax`] for malformed Turtle or non-UTF-8 input,
/// [`OntologyError::ResourceLimit`] when a budget is exceeded,
/// [`OntologyError::NamedGraph`] when a quad names a graph,
/// [`OntologyError::TripleTerm`] for an RDF 1.2 triple term, and
/// [`OntologyError::BaseDirection`] for a literal carrying a base direction.
pub fn parse_turtle_terms(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<Vec<TripleTerms>, OntologyError> {
    let mut triples: Vec<TripleTerms> = Vec::new();
    let mut seen: BTreeSet<TripleTerms> = BTreeSet::new();
    read_default_graph(source, limits, |quad, blank_labels| {
        let subject = match &quad.subject {
            NamedOrBlankNode::NamedNode(node) => node.as_str().to_owned(),
            NamedOrBlankNode::BlankNode(node) => blank_labels.label(node.as_str()),
        };
        let candidate = TripleTerms {
            subject,
            predicate: quad.predicate.as_str().to_owned(),
            object: object_view(&quad.object, blank_labels)?,
        };
        if seen.insert(candidate.clone()) {
            enforce_statement_budget(seen.len(), limits)?;
            triples.push(candidate);
        }
        Ok(())
    })?;
    Ok(triples)
}

/// Return the prefix table the document itself declares.
///
/// The table is read from the Turtle parser, not from the text: `@prefix` and
/// SPARQL-style `PREFIX` both declare (Turtle 1.1 §6.5 `prefixID` and
/// `sparqlPrefix`), a namespace written relative to `@base` is resolved, and a
/// directive-looking line inside a literal declares nothing.
///
/// When a document binds one prefix twice the **final** binding is reported,
/// and it is the binding [`parse_turtle`] renders every term through. Turtle
/// scopes a binding from its directive to the next one, but the text form
/// carries one table for the whole document, and under it a prefixed name must
/// expand to exactly one IRI. A term written under a superseded binding
/// therefore renders as a full `<...>` IRI — correct, never a guess — and a
/// document that binds each prefix once renders exactly as it always has,
/// wherever in the document the directive sits.
///
/// # Errors
///
/// Returns [`OntologyError::ResourceLimit`] when `source` exceeds
/// `limits.max_source_bytes`, and [`OntologyError::Syntax`] when the document
/// does not parse: a table read from half a document would be a guess.
pub fn declared_prefixes(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<BTreeMap<String, String>, OntologyError> {
    read_default_graph(source, limits, |_, _| Ok(()))
}

/// Drive the Turtle parser over `source`, handing each default-graph quad on.
///
/// The one parse loop under both views: the source budget, syntax errors, the
/// named-graph refusal and blank node labelling cannot drift apart. Returns
/// the prefix table the parser holds at the end of the document.
fn read_default_graph(
    source: &[u8],
    limits: OntologyLimits,
    mut visit: impl FnMut(&Quad, &mut BlankLabels) -> Result<(), OntologyError>,
) -> Result<BTreeMap<String, String>, OntologyError> {
    if source.len() > limits.max_source_bytes {
        return Err(OntologyError::ResourceLimit {
            resource: "source_bytes",
            limit: limits.max_source_bytes,
            observed: source.len(),
        });
    }
    // A Turtle document is UTF-8 (Turtle 1.1 §6.1). The tokenizer validates
    // the bytes of each term but steps over a comment without decoding it, so
    // the document as a whole is checked here, for both views alike.
    std::str::from_utf8(source).map_err(|error| OntologyError::Syntax(error.to_string()))?;
    let mut blank_labels = BlankLabels::for_document(source);
    let mut parser = RdfParser::from_format(RdfFormat::Turtle).for_slice(source);
    // `by_ref`: the parser is read again after the last quad, for the prefixes
    // it saw.
    for quad in parser.by_ref() {
        let quad = quad.map_err(|error| OntologyError::Syntax(error.to_string()))?;
        if !matches!(quad.graph_name, GraphName::DefaultGraph) {
            return Err(OntologyError::NamedGraph(quad.graph_name.to_string()));
        }
        visit(&quad, &mut blank_labels)?;
    }
    Ok(parser
        .prefixes()
        .map(|(prefix, namespace)| (prefix.to_owned(), namespace.to_owned()))
        .collect())
}

/// Refuse the first distinct statement beyond `limits.max_statements`.
fn enforce_statement_budget(observed: usize, limits: OntologyLimits) -> Result<(), OntologyError> {
    if observed > limits.max_statements {
        return Err(OntologyError::ResourceLimit {
            resource: "statements",
            limit: limits.max_statements,
            observed,
        });
    }
    Ok(())
}

/// A term held until the document's prefix table is complete.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StagedTerm {
    /// A full IRI, abbreviated at render time.
    Iri(String),
    /// Text that renders as it stands: a literal's lexical form or a blank label.
    Verbatim(String),
}

impl StagedTerm {
    fn render(self, prefixes: &BTreeMap<String, String>) -> String {
        match self {
            Self::Iri(iri) => abbreviate(&iri, prefixes),
            Self::Verbatim(text) => text,
        }
    }
}

/// One text-view triple before abbreviation.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StagedTriple {
    subject: StagedTerm,
    predicate: String,
    object: StagedTerm,
}

/// Render an object-position term, refusing what the view cannot carry.
fn object_view(term: &Term, blank_labels: &mut BlankLabels) -> Result<TermView, OntologyError> {
    match term {
        Term::NamedNode(node) => Ok(TermView::Iri(node.as_str().to_owned())),
        Term::BlankNode(node) => Ok(TermView::Blank(blank_labels.label(node.as_str()))),
        Term::Literal(literal) => literal_view(literal),
        Term::Triple(_) => Err(OntologyError::TripleTerm { position: "object" }),
    }
}

/// Render a literal with the typing the routing text contract erases.
fn literal_view(literal: &oxrdf::Literal) -> Result<TermView, OntologyError> {
    if literal.direction().is_some() {
        return Err(OntologyError::BaseDirection { position: "object" });
    }
    let value = literal.value().to_owned();
    if let Some(language) = literal.language() {
        return Ok(TermView::Literal {
            value,
            datatype: None,
            // Lowercased for parity with rdflib, which normalizes a language
            // tag to lowercase on construction.
            language: Some(language.to_ascii_lowercase()),
        });
    }
    let datatype = literal.datatype().as_str();
    if datatype == XSD_STRING {
        return Ok(TermView::Literal {
            value,
            datatype: None,
            language: None,
        });
    }
    Ok(TermView::Literal {
        value,
        datatype: Some(datatype.to_owned()),
        language: None,
    })
}

/// Shorten an IRI only through a prefix the document declared.
///
/// The longest declared namespace that starts the IRI wins; between two
/// prefixes bound to the same namespace, the one that sorts first does.
#[must_use]
pub fn abbreviate(iri: &str, prefixes: &BTreeMap<String, String>) -> String {
    let mut best: Option<(&String, &String)> = None;
    for (prefix, namespace) in prefixes {
        if !iri.starts_with(namespace.as_str()) {
            continue;
        }
        if best.is_none_or(|(_, current)| namespace.len() > current.len()) {
            best = Some((prefix, namespace));
        }
    }
    match best {
        Some((prefix, namespace)) => format!("{prefix}:{}", &iri[namespace.len()..]),
        None => format!("<{iri}>"),
    }
}

/// Document-scoped blank node labels, numbered by first appearance.
///
/// RDF 1.2 Concepts §3.5: a blank node label "is local to the document" — it
/// carries no meaning, and §3.7 makes graph equality **isomorphism**, not
/// textual equality. The parser's own identifiers are unusable as they stand:
/// `[]` and collection nodes get a fresh random id on every parse, so the same
/// bytes would read to different text each time, and an authored `_:r` is
/// passed through verbatim, so two documents that both say `_:r` would name
/// one node once a caller adds both to one graph (the app does exactly that
/// with its ontology modules).
///
/// So every blank node — authored or anonymous — is relabelled
/// `_:d<scope>b<n>`: `<scope>` is a 64-bit FNV-1a hash of the source bytes in
/// hex, `<n>` counts blank nodes by first appearance in subject-then-object
/// statement order. The label is therefore identical across parses and across
/// both views of one document, and distinct between documents. It is still
/// **not** a canonical label: a different serialization of the same graph gets
/// different labels, so graphs are compared by isomorphism, never by string
/// equality, whenever a document contains a blank node.
///
/// The scope is an identifier, not a security boundary: FNV-1a is not
/// collision-resistant, and two documents engineered to collide would share
/// labels. The reader's inputs are the repository's own ontology modules.
///
/// The scope follows the bytes, not the path: the same bytes stored at two
/// paths read to the same labels, so a caller that merges both copies into one
/// graph merges their blank nodes too. For identical documents that is the
/// same graph either way.
///
/// ## Alternatives considered
///
/// * **RDFC-1.0 canonical labels.** Would make the text comparable across
///   serializations. Not done: it is a substantial algorithm (graph hashing
///   plus a fixpoint over structurally-equivalent nodes), and the comparison
///   this reader feeds is already handled by isomorphism.
/// * **Skolem IRIs** — Concepts Appendix B: mint
///   `https://<authority>/.well-known/genid/<id>` per blank node, which is what
///   makes the replacement recognisable outside the system. Not done: it
///   *changes the graph*, adding entailments the source did not assert, and this
///   reader's job is to report the document, not to strengthen it.
///
/// RDF 1.2 §7's symmetric and generalized triples are not accepted either. The
/// spec is explicit that they are non-standard and that "there is no requirement
/// for any RDF tool to accept, process, or produce anything beyond standard
/// normative RDF" — so refusing them (see [`OntologyError::TripleTerm`]) is
/// conformant rather than a gap.
#[derive(Debug)]
struct BlankLabels {
    scope: u64,
    labels: HashMap<String, String>,
}

impl BlankLabels {
    fn for_document(source: &[u8]) -> Self {
        Self {
            scope: fnv1a_64(source),
            labels: HashMap::new(),
        }
    }

    /// The label for the parser's blank node identifier `id`.
    fn label(&mut self, id: &str) -> String {
        if let Some(label) = self.labels.get(id) {
            return label.clone();
        }
        let label = format!("_:d{:016x}b{}", self.scope, self.labels.len());
        self.labels.insert(id.to_owned(), label.clone());
        label
    }
}

/// 64-bit FNV-1a (Fowler–Noll–Vo, <http://www.isthe.com/chongo/tech/comp/fnv/>).
///
/// Chosen over `std`'s `DefaultHasher` because that algorithm is explicitly
/// unspecified and may change between Rust releases, which would silently
/// rename every blank node; this one is fixed by its two published constants.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// `rdf:type` is Turtle's `a`; everything else abbreviates as usual.
#[must_use]
pub fn predicate_text(iri: &str, prefixes: &BTreeMap<String, String>) -> String {
    if iri == RDF_TYPE {
        "a".to_owned()
    } else {
        abbreviate(iri, prefixes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ParsedOntology {
        parse_turtle(text.as_bytes(), OntologyLimits::default()).expect("parses")
    }

    #[test]
    fn renders_type_as_a_and_abbreviates_declared_prefixes() {
        let ontology = parse(
            "@prefix ex: <https://example.org/ns#> .\n\
             @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
             ex:ROAS a ex:Metric ;\n    ex:label \"Return on ad spend\" .\n",
        );
        let rendered: Vec<String> = ontology.triples().iter().map(TripleText::to_line).collect();
        assert!(rendered.contains(&"ex:ROAS\ta\tex:Metric".to_owned()));
        assert!(
            rendered.contains(&"ex:ROAS\tex:label\tReturn on ad spend".to_owned()),
            "{rendered:?}"
        );
    }

    #[test]
    fn leaves_an_undeclared_namespace_as_a_full_iri() {
        let ontology =
            parse("<https://example.org/ns> a <http://www.w3.org/2002/07/owl#Ontology> .\n");
        let rendered: Vec<String> = ontology.triples().iter().map(TripleText::to_line).collect();
        assert_eq!(
            rendered,
            vec![
                "<https://example.org/ns>\ta\t<http://www.w3.org/2002/07/owl#Ontology>".to_owned()
            ]
        );
    }

    #[test]
    fn reads_a_multi_line_literal_the_subset_parser_corrupts() {
        let ontology = parse(
            "@prefix ex: <https://example.org/ns#> .\n\
             ex:ROAS a ex:Metric ;\n    ex:note \"\"\"line one\n    line two\"\"\" .\n",
        );
        let rendered: Vec<String> = ontology.triples().iter().map(TripleText::to_line).collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("line one\n    line two")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("\tline\t")),
            "must not mint the phantom predicate the regex parser produces: {rendered:?}"
        );
    }

    #[test]
    fn index_answers_subject_predicate_and_object_lookups() {
        let ontology = parse(
            "@prefix ex: <https://example.org/ns#> .\n\
             ex:ROAS a ex:Metric .\n\
             ex:ROAS ex:owner \"revops\" .\n",
        );
        let index = ontology.index();
        assert_eq!(index.by_subject("ex:ROAS").len(), 2);
        assert_eq!(index.by_predicate("a").len(), 1);
        assert_eq!(index.by_object("ex:Metric").len(), 1);
        assert!(index.by_subject("ex:absent").is_empty());
    }

    #[test]
    fn reads_a_blank_node_and_labels_it_within_the_document() {
        // RDF 1.2 Concepts §3.5: the label is document-local and carries no
        // meaning, so this asserts the node is *represented*, not that any
        // particular label was chosen.
        let ontology = parse_turtle(
            b"@prefix ex: <https://example.org/ns#> .\n\
              [] a ex:Metric ; ex:owner \"revops\" .\n",
            OntologyLimits::default(),
        )
        .expect("blank nodes must read");

        let labels: Vec<&str> = ontology
            .triples()
            .iter()
            .map(|triple| triple.subject.as_str())
            .collect();
        assert_eq!(labels.len(), 2, "{labels:?}");
        assert!(
            labels.iter().all(|label| label.starts_with("_:")),
            "a blank node must render as a labelled term, got {labels:?}"
        );
        assert_eq!(
            labels[0], labels[1],
            "one blank node keeps one label within a document"
        );
    }

    #[test]
    fn enforces_the_statement_budget() {
        let source = b"@prefix ex: <https://example.org/ns#> .\nex:a a ex:B .\nex:c a ex:D .\n";
        let error = parse_turtle(
            source,
            OntologyLimits {
                max_source_bytes: 1024,
                max_statements: 1,
            },
        )
        .expect_err("budget is enforced");
        assert!(
            matches!(
                error,
                OntologyError::ResourceLimit {
                    resource: "statements",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn term_view_keeps_full_iris_and_literal_typing() {
        let triples = parse_turtle_terms(
            b"@prefix ex: <https://example.org/ns#> .\n\
              @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
              ex:ROAS a ex:Metric ;\n\
              ex:note \"plain\" ;\n\
              ex:since \"2026-09-16\"^^xsd:date ;\n\
              ex:summary \"Hello\"@EN .\n",
            OntologyLimits::default(),
        )
        .expect("parses");
        assert_eq!(triples.len(), 4, "{triples:?}");
        let by_predicate = |predicate: &str| {
            triples
                .iter()
                .find(|triple| triple.predicate.ends_with(predicate))
                .unwrap_or_else(|| panic!("missing predicate {predicate}: {triples:?}"))
        };
        let type_triple = by_predicate("rdf-syntax-ns#type");
        assert_eq!(type_triple.subject, "https://example.org/ns#ROAS");
        assert_eq!(
            type_triple.predicate, "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "rdf:type stays a full IRI in the term view"
        );
        assert_eq!(
            type_triple.object,
            TermView::Iri("https://example.org/ns#Metric".to_owned())
        );
        assert_eq!(
            by_predicate("#note").object,
            TermView::Literal {
                value: "plain".to_owned(),
                datatype: None,
                language: None,
            }
        );
        assert_eq!(
            by_predicate("#since").object,
            TermView::Literal {
                value: "2026-09-16".to_owned(),
                datatype: Some("http://www.w3.org/2001/XMLSchema#date".to_owned()),
                language: None,
            }
        );
        assert_eq!(
            by_predicate("#summary").object,
            TermView::Literal {
                value: "Hello".to_owned(),
                datatype: None,
                // Lowercased, matching rdflib's normalization.
                language: Some("en".to_owned()),
            }
        );
    }

    #[test]
    fn term_view_deduplicates_and_refuses_a_base_direction() {
        let triples = parse_turtle_terms(
            b"<urn:a> <urn:p> \"x\" .\n<urn:a> <urn:p> \"x\" .\n",
            OntologyLimits::default(),
        )
        .expect("parses");
        assert_eq!(triples.len(), 1, "{triples:?}");

        let error = parse_turtle_terms(
            b"<urn:a> <urn:p> \"x\"@en--ltr .\n",
            OntologyLimits::default(),
        )
        .expect_err("a base direction has no representation in the term view");
        assert!(
            matches!(error, OntologyError::BaseDirection { position: "object" }),
            "{error:?}"
        );
    }
}
