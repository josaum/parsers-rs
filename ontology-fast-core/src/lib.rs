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

use std::collections::{BTreeMap, HashMap, HashSet};

use oxrdf::{GraphName, NamedOrBlankNode, Quad, Term};
use oxttl::TurtleParser;
use thiserror::Error;

/// The RDF type IRI, rendered as Turtle's own `a` keyword.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The XSD `string` datatype IRI (XML Schema Part 2 §3.2.1).
///
/// RDF 1.2 Concepts §3.3 makes a simple literal and `"..."^^xsd:string` the
/// same term, and `oxttl` reports both with this datatype. The term view
/// maps it back to "no datatype", which is how the app's previous reader
/// (rdflib) reports a simple literal. A source that writes `^^xsd:string`
/// explicitly therefore reads as a simple literal here while rdflib keeps the
/// datatype: a recorded contract difference, not a parse failure.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Deepest nesting of brackets one document may use.
///
/// The Turtle parser keeps a few hundred bytes of state for every `[ ... ]`,
/// `( ... )`, `<< ... >>` or `{| ... |}` it is inside, so a megabyte of opening
/// brackets would otherwise cost hundreds of megabytes before a single
/// statement is read. Ten thousand levels is far beyond any authored
/// ontology and holds that state to a few megabytes. A document that nests
/// deeper is refused with [`OntologyError::ResourceLimit`] naming
/// `nesting_depth`, by every entry point, before it is parsed.
pub const MAX_NESTING_DEPTH: usize = 10_000;

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
    by_subject: IndexColumn,
    by_predicate: IndexColumn,
    by_object: IndexColumn,
}

/// One position of the index: every distinct term with its row ordinals.
///
/// Stored as four flat buffers rather than a map of vectors, so a column costs
/// four bytes per row plus the text of each distinct term once, whatever the
/// number of distinct terms.
#[derive(Clone, Debug, Default)]
struct IndexColumn {
    /// The distinct terms, concatenated in ascending order.
    text: String,
    /// `text_ends[i]` is where term `i` ends in `text`; it starts where term
    /// `i - 1` ends.
    text_ends: Vec<usize>,
    /// `run_ends[i]` is where the ordinals of term `i` end in `ordinals`.
    run_ends: Vec<usize>,
    /// Row ordinals grouped by term, ascending within each group.
    ordinals: Vec<u32>,
}

impl IndexColumn {
    /// Build the column for one position of `rows`.
    ///
    /// `rows[ordinal][position]` is a term number below `terms.len()`, and two
    /// different numbers never name the same text.
    fn build(rows: &[[u32; 3]], position: usize, terms: &[String]) -> Self {
        // Rows per term, then the distinct terms of this position in text order.
        let mut counts = vec![0_usize; terms.len()];
        for row in rows {
            counts[row[position] as usize] += 1;
        }
        let mut present: Vec<u32> = rows.iter().map(|row| row[position]).collect();
        present.sort_unstable();
        present.dedup();
        present.sort_unstable_by(|left, right| terms[*left as usize].cmp(&terms[*right as usize]));

        let mut column = Self {
            text: String::with_capacity(
                present.iter().map(|term| terms[*term as usize].len()).sum(),
            ),
            text_ends: Vec::with_capacity(present.len()),
            run_ends: Vec::with_capacity(present.len()),
            ordinals: vec![0; rows.len()],
        };
        // `counts` turns from a row count into the next free slot of the run.
        let mut filled = 0_usize;
        for term in present {
            let term = term as usize;
            column.text.push_str(&terms[term]);
            column.text_ends.push(column.text.len());
            let rows_of_term = counts[term];
            counts[term] = filled;
            filled += rows_of_term;
            column.run_ends.push(filled);
        }
        for (ordinal, row) in (0_u32..).zip(rows) {
            let slot = &mut counts[row[position] as usize];
            column.ordinals[*slot] = ordinal;
            *slot += 1;
        }
        column
    }

    fn term(&self, rank: usize) -> &str {
        let start = if rank == 0 {
            0
        } else {
            self.text_ends[rank - 1]
        };
        &self.text[start..self.text_ends[rank]]
    }

    /// Row ordinals whose term at this position is exactly `term`.
    fn rows(&self, term: &str) -> &[u32] {
        // Binary search over the sorted distinct terms.
        let (mut low, mut high) = (0, self.text_ends.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.term(middle).cmp(term) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let start = if middle == 0 {
                        0
                    } else {
                        self.run_ends[middle - 1]
                    };
                    return &self.ordinals[start..self.run_ends[middle]];
                }
            }
        }
        &[]
    }
}

impl OntologyIndex {
    /// Row ordinals whose subject is exactly `subject`.
    #[must_use]
    pub fn by_subject(&self, subject: &str) -> &[u32] {
        self.by_subject.rows(subject)
    }

    /// Row ordinals whose predicate is exactly `predicate`.
    #[must_use]
    pub fn by_predicate(&self, predicate: &str) -> &[u32] {
        self.by_predicate.rows(predicate)
    }

    /// Row ordinals whose object is exactly `object`.
    #[must_use]
    pub fn by_object(&self, object: &str) -> &[u32] {
        self.by_object.rows(object)
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
                // Every run is filled in ascending ordinal order, which is
                // what makes the binary search valid.
                rest.iter()
                    .all(|column| column.binary_search(ordinal).is_ok())
            })
            .collect()
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
/// [`OntologyError::ResourceLimit`] when a budget or [`MAX_NESTING_DEPTH`] is
/// exceeded, [`OntologyError::NamedGraph`] when a quad names a graph, and
/// [`OntologyError::TripleTerm`] for an RDF 1.2 triple term. A base direction
/// is not refused here: the text form keeps a literal's lexical form only, so
/// direction is erased exactly as the language tag and datatype are.
pub fn parse_turtle(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<ParsedOntology, OntologyError> {
    // Rendering needs the document's final prefix table, which exists only
    // once the parser has reached the end, so rows are staged first. A staged
    // row is three term numbers: each distinct term is held once, however many
    // statements repeat it. The stage is deduplicated and held to the
    // statement budget, which keeps retained memory bounded by the same
    // ceiling as the result.
    let mut terms = StagedTerms::default();
    let mut staged: Vec<[u32; 3]> = Vec::new();
    let mut staged_seen: HashSet<[u32; 3]> = HashSet::new();
    let prefixes = read_default_graph(source, limits, |quad, blank_labels| {
        // Subject before object: blank nodes are numbered by first appearance,
        // and both views must meet them in the same order.
        let subject = match &quad.subject {
            NamedOrBlankNode::NamedNode(node) => terms.number(TermKind::Iri, node.as_str())?,
            NamedOrBlankNode::BlankNode(node) => {
                terms.number(TermKind::Verbatim, blank_labels.label(node.as_str()))?
            }
        };
        let object = match &quad.object {
            Term::NamedNode(node) => terms.number(TermKind::Iri, node.as_str())?,
            Term::Literal(literal) => terms.number(TermKind::Verbatim, literal.value())?,
            Term::BlankNode(node) => {
                terms.number(TermKind::Verbatim, blank_labels.label(node.as_str()))?
            }
            Term::Triple(_) => return Err(OntologyError::TripleTerm { position: "object" }),
        };
        let predicate = terms.number(TermKind::Predicate, quad.predicate.as_str())?;
        let candidate = [subject, predicate, object];
        if staged_seen.insert(candidate) {
            enforce_statement_budget(staged_seen.len(), limits)?;
            staged.push(candidate);
        }
        Ok(())
    })?;
    drop(staged_seen);

    // Each distinct term is rendered once. Two staged terms can render to one
    // text (a literal whose lexical form equals an abbreviated IRI), so every
    // term is mapped to the first term with its text, and the rows are
    // deduplicated again only when such a pair exists.
    let abbreviator = Abbreviator::new(&prefixes);
    let rendered = terms.render(&abbreviator);
    let mut first_with_text: HashMap<&str, u32> = HashMap::with_capacity(rendered.len());
    let mut folded = false;
    let canonical: Vec<u32> = (0_u32..)
        .zip(&rendered)
        .map(|(number, text)| {
            let first = *first_with_text.entry(text.as_str()).or_insert(number);
            folded |= first != number;
            first
        })
        .collect();
    drop(first_with_text);
    let mut rows = staged;
    if folded {
        let mut seen: HashSet<[u32; 3]> = HashSet::with_capacity(rows.len());
        rows.retain_mut(|row| {
            *row = row.map(|number| canonical[number as usize]);
            seen.insert(*row)
        });
    }
    drop(canonical);

    if u32::try_from(rows.len()).is_err() {
        return Err(OntologyError::ResourceLimit {
            resource: "index_ordinals",
            limit: u32::MAX as usize,
            observed: rows.len(),
        });
    }
    let triples: Vec<TripleText> = rows
        .iter()
        .map(|[subject, predicate, object]| TripleText {
            subject: rendered[*subject as usize].clone(),
            predicate: rendered[*predicate as usize].clone(),
            object: rendered[*object as usize].clone(),
        })
        .collect();
    let index = OntologyIndex {
        by_subject: IndexColumn::build(&rows, 0, &rendered),
        by_predicate: IndexColumn::build(&rows, 1, &rendered),
        by_object: IndexColumn::build(&rows, 2, &rendered),
    };

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
/// [`OntologyError::ResourceLimit`] when a budget or [`MAX_NESTING_DEPTH`] is
/// exceeded, [`OntologyError::NamedGraph`] when a quad names a graph,
/// [`OntologyError::TripleTerm`] for an RDF 1.2 triple term, and
/// [`OntologyError::BaseDirection`] for a literal carrying a base direction.
pub fn parse_turtle_terms(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<Vec<TripleTerms>, OntologyError> {
    // Deduplication compares six numbers per statement instead of the
    // statement's text: each distinct string is numbered once.
    let mut triples: Vec<TripleTerms> = Vec::new();
    let mut strings = StringNumbers::default();
    let mut seen: HashSet<[u32; 6]> = HashSet::new();
    read_default_graph(source, limits, |quad, blank_labels| {
        let subject = match &quad.subject {
            NamedOrBlankNode::NamedNode(node) => node.as_str(),
            NamedOrBlankNode::BlankNode(node) => blank_labels.label(node.as_str()),
        };
        let subject_number = strings.number(subject)?;
        let subject = subject.to_owned();
        let predicate = quad.predicate.as_str();
        let object = object_view(&quad.object, blank_labels)?;
        // Zero is "absent"; a present string is its number plus one.
        let (kind, value, datatype, language) = match &object {
            TermView::Iri(iri) => (0, strings.number(iri)?, 0, 0),
            TermView::Blank(label) => (1, strings.number(label)?, 0, 0),
            TermView::Literal {
                value,
                datatype,
                language,
            } => (
                2,
                strings.number(value)?,
                strings.optional_number(datatype.as_deref())?,
                strings.optional_number(language.as_deref())?,
            ),
        };
        let key = [
            subject_number,
            strings.number(predicate)?,
            kind,
            value,
            datatype,
            language,
        ];
        if seen.insert(key) {
            enforce_statement_budget(seen.len(), limits)?;
            triples.push(TripleTerms {
                subject,
                predicate: predicate.to_owned(),
                object,
            });
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
/// `limits.max_source_bytes` or [`MAX_NESTING_DEPTH`], and [`OntologyError::Syntax`] when the document
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
    if nests_deeper_than(source, MAX_NESTING_DEPTH) {
        return Err(OntologyError::ResourceLimit {
            resource: "nesting_depth",
            limit: MAX_NESTING_DEPTH,
            observed: MAX_NESTING_DEPTH + 1,
        });
    }
    let mut blank_labels = BlankLabels::for_document(source);
    let mut parser = TurtleParser::new().for_slice(source);
    // `by_ref`: the parser is read again after the last triple, for the
    // prefixes it saw.
    for triple in parser.by_ref() {
        // Turtle has no graph syntax, so every statement is in the default
        // graph; `OntologyError::NamedGraph` stays for a future quad format.
        let quad = triple
            .map_err(|error| OntologyError::Syntax(error.to_string()))?
            .in_graph(GraphName::DefaultGraph);
        visit(&quad, &mut blank_labels)?;
    }
    Ok(parser
        .prefixes()
        .map(|(prefix, namespace)| (prefix.to_owned(), namespace.to_owned()))
        .collect())
}

/// Whether `source` opens more than `limit` brackets inside one another.
///
/// A scan of the bytes, not a parse: it steps over what cannot hold a bracket
/// (comments, quoted strings, IRIs and escaped characters) and counts `(`,
/// `[`, `{` and `<<` against `)`, `]`, `}` and `>>`. It follows the Turtle
/// tokenizer for as long as the document is well formed, and the parser stops
/// at the first token that is not, so the count is the parser's own depth for
/// every byte the parser would go on to read.
fn nests_deeper_than(source: &[u8], limit: usize) -> bool {
    let mut depth = 0_usize;
    let mut at = 0_usize;
    while let Some(byte) = source.get(at).copied() {
        at += 1;
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            // An escaped character in a prefixed name, `ex:a\(b`.
            b'\\' => at += 1,
            b'#' => {
                while source
                    .get(at)
                    .is_some_and(|next| !matches!(next, b'\n' | b'\r'))
                {
                    at += 1;
                }
            }
            b'<' if source.get(at) == Some(&b'<') => {
                at += 1;
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b'<' => {
                // An IRI reference runs to `>`; it cannot hold white space or
                // any of the other bytes that end the scan here.
                while let Some(next) = source.get(at).copied() {
                    if next <= b' '
                        || matches!(next, b'<' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`')
                    {
                        break;
                    }
                    at += 1;
                    if next == b'>' {
                        break;
                    }
                }
            }
            b'>' if source.get(at) == Some(&b'>') => {
                at += 1;
                depth = depth.saturating_sub(1);
            }
            b'"' | b'\'' => at = end_of_string(source, at, byte),
            _ => {}
        }
    }
    false
}

/// The offset just past the quoted string whose opening `quote` ends at `at`.
fn end_of_string(source: &[u8], mut at: usize, quote: u8) -> usize {
    let long = source.get(at) == Some(&quote) && source.get(at + 1) == Some(&quote);
    if long {
        at += 2;
    }
    while let Some(byte) = source.get(at).copied() {
        at += 1;
        if byte == b'\\' {
            at += 1;
        } else if long {
            if byte == quote && source.get(at) == Some(&quote) && source.get(at + 1) == Some(&quote)
            {
                return at + 2;
            }
        } else if byte == quote || byte == b'\n' || byte == b'\r' {
            // A short string ends at its quote and cannot span a line.
            return at;
        }
    }
    at
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

/// How a staged term renders once the prefix table is complete.
///
/// Two terms are one term only when both the kind and the text agree: the IRI
/// `urn:x` and the literal `"urn:x"` are different terms.
#[derive(Clone, Copy, Debug)]
enum TermKind {
    /// A full IRI in subject or object position, abbreviated at render time.
    Iri = 0,
    /// Text that renders as it stands: a literal's lexical form or a blank label.
    Verbatim = 1,
    /// A full IRI in predicate position: `rdf:type` renders as `a`.
    Predicate = 2,
}

/// The distinct terms of one document, numbered by first appearance.
#[derive(Debug, Default)]
struct StagedTerms {
    /// One table per [`TermKind`]; the numbers are unique across all three.
    by_kind: [HashMap<String, u32>; 3],
    count: u32,
}

impl StagedTerms {
    /// The number of the term `text` of kind `kind`, assigning the next on first sight.
    fn number(&mut self, kind: TermKind, text: &str) -> Result<u32, OntologyError> {
        let table = &mut self.by_kind[kind as usize];
        if let Some(number) = table.get(text) {
            return Ok(*number);
        }
        let number = self.count;
        self.count = next_number(number)?;
        table.insert(text.to_owned(), number);
        Ok(number)
    }

    /// The text form of every term, at the index of its number.
    fn render(self, abbreviator: &Abbreviator<'_>) -> Vec<String> {
        let mut rendered = vec![String::new(); self.count as usize];
        let [iris, verbatim, predicates] = self.by_kind;
        // The tables are walked in hash order, but every text lands in the
        // slot of its own number, so the result does not depend on that order.
        for (iri, number) in iris {
            rendered[number as usize] = abbreviator.abbreviate(&iri);
        }
        for (text, number) in verbatim {
            rendered[number as usize] = text;
        }
        for (iri, number) in predicates {
            rendered[number as usize] = if iri == RDF_TYPE {
                "a".to_owned()
            } else {
                abbreviator.abbreviate(&iri)
            };
        }
        rendered
    }
}

/// Distinct strings numbered by first appearance, for the term view's deduplication.
#[derive(Debug, Default)]
struct StringNumbers {
    numbers: HashMap<String, u32>,
    count: u32,
}

impl StringNumbers {
    /// The number of `text`, assigning the next on first sight. Never zero.
    fn number(&mut self, text: &str) -> Result<u32, OntologyError> {
        if let Some(number) = self.numbers.get(text) {
            return Ok(*number);
        }
        self.count = next_number(self.count)?;
        self.numbers.insert(text.to_owned(), self.count);
        Ok(self.count)
    }

    /// As [`Self::number`], with zero for an absent string.
    fn optional_number(&mut self, text: Option<&str>) -> Result<u32, OntologyError> {
        text.map_or(Ok(0), |text| self.number(text))
    }
}

/// The number after `number`, refused once 32 bits are exhausted.
///
/// Row ordinals are 32-bit, so a document with this many distinct terms could
/// not be indexed either; it is refused under the same resource name.
fn next_number(number: u32) -> Result<u32, OntologyError> {
    number.checked_add(1).ok_or(OntologyError::ResourceLimit {
        resource: "index_ordinals",
        limit: u32::MAX as usize,
        observed: u32::MAX as usize + 1,
    })
}

/// A prefix table arranged for longest-namespace lookup.
///
/// [`abbreviate`] tests every declared prefix against one IRI. Rendering a
/// document asks the same question once per distinct IRI, so the namespaces
/// are sorted once and each IRI is answered by one binary search.
#[derive(Debug)]
struct Abbreviator<'a> {
    /// `(namespace, prefix)`, ascending by namespace, one entry per namespace:
    /// the prefix that sorts first, as in [`abbreviate`].
    namespaces: Vec<(&'a str, &'a str)>,
    /// `parents[i]` is the index of the longest other namespace that starts
    /// namespace `i`, when there is one.
    parents: Vec<Option<usize>>,
}

impl<'a> Abbreviator<'a> {
    fn new(prefixes: &'a BTreeMap<String, String>) -> Self {
        let mut namespaces: Vec<(&str, &str)> = prefixes
            .iter()
            .map(|(prefix, namespace)| (namespace.as_str(), prefix.as_str()))
            .collect();
        // Stable, and the map iterates in prefix order, so the first entry of
        // each run of equal namespaces carries the prefix that sorts first.
        namespaces.sort_by_key(|(namespace, _)| *namespace);
        namespaces.dedup_by_key(|(namespace, _)| *namespace);
        // In sorted order every namespace follows the namespaces that start
        // it, so one pass with a stack of the still-open ones finds each
        // parent: whatever does not start the current entry starts no later
        // entry either.
        let mut parents = Vec::with_capacity(namespaces.len());
        let mut open: Vec<usize> = Vec::new();
        for (index, (namespace, _)) in namespaces.iter().enumerate() {
            while let Some(top) = open.last().copied() {
                if namespaces
                    .get(top)
                    .is_some_and(|(outer, _)| namespace.starts_with(outer))
                {
                    break;
                }
                open.pop();
            }
            parents.push(open.last().copied());
            open.push(index);
        }
        Self {
            namespaces,
            parents,
        }
    }

    /// The longest declared namespace that starts `iri`, with its prefix.
    fn longest_namespace(&self, iri: &str) -> Option<(&'a str, &'a str)> {
        // Every namespace that starts `iri` sorts at or before it, and so does
        // `nearest`, the greatest namespace at or before `iri`. `nearest` sits
        // between the two, so every answer starts `nearest` as well: the
        // answer is `nearest` itself or one of its parents, and a parent
        // starts `iri` exactly when it is no longer than what `nearest` and
        // `iri` share. One search, then lengths only.
        let after = self
            .namespaces
            .partition_point(|(namespace, _)| *namespace <= iri);
        let mut at = after.checked_sub(1)?;
        let (nearest, _) = *self.namespaces.get(at)?;
        let shared = nearest
            .bytes()
            .zip(iri.bytes())
            .take_while(|(left, right)| left == right)
            .count();
        loop {
            let (namespace, prefix) = *self.namespaces.get(at)?;
            if namespace.len() <= shared {
                return Some((namespace, prefix));
            }
            at = (*self.parents.get(at)?)?;
        }
    }

    /// As [`abbreviate`], for this table.
    fn abbreviate(&self, iri: &str) -> String {
        match self.longest_namespace(iri) {
            Some((namespace, prefix)) => format!("{prefix}:{}", &iri[namespace.len()..]),
            None => format!("<{iri}>"),
        }
    }
}

/// Render an object-position term, refusing what the view cannot carry.
fn object_view(term: &Term, blank_labels: &mut BlankLabels) -> Result<TermView, OntologyError> {
    match term {
        Term::NamedNode(node) => Ok(TermView::Iri(node.as_str().to_owned())),
        Term::BlankNode(node) => Ok(TermView::Blank(
            blank_labels.label(node.as_str()).to_owned(),
        )),
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
    fn label(&mut self, id: &str) -> &str {
        if !self.labels.contains_key(id) {
            let label = format!("_:d{:016x}b{}", self.scope, self.labels.len());
            self.labels.insert(id.to_owned(), label);
        }
        // Present by now; the empty fallback is unreachable.
        self.labels.get(id).map_or("", String::as_str)
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

    #[test]
    fn the_sorted_namespace_lookup_agrees_with_the_linear_scan() {
        // Nested namespaces, shared namespaces, the empty namespace and
        // multi-byte text, probed with IRIs built from the same alphabet.
        let pieces = [
            "", "a", "ab", "a/", "a/b", "a/b/", "b", "é", "éa", "a/é", "ab/c",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize % bound
        };
        for round in 0..2_000 {
            let mut prefixes = BTreeMap::new();
            for slot in 0..next(8) {
                let namespace = format!(
                    "{}{}",
                    pieces[next(pieces.len())],
                    pieces[next(pieces.len())]
                );
                prefixes.insert(format!("p{}", (slot * 7 + round) % 10), namespace);
            }
            let abbreviator = Abbreviator::new(&prefixes);
            for _ in 0..20 {
                let iri = format!(
                    "{}{}{}",
                    pieces[next(pieces.len())],
                    pieces[next(pieces.len())],
                    pieces[next(pieces.len())]
                );
                assert_eq!(
                    abbreviator.abbreviate(&iri),
                    abbreviate(&iri, &prefixes),
                    "iri {iri:?} under {prefixes:?}"
                );
            }
        }
    }

    #[test]
    fn the_nesting_scan_counts_brackets_only_where_the_tokenizer_would() {
        let deeper_than = |text: &str, limit| nests_deeper_than(text.as_bytes(), limit);
        assert!(deeper_than("<a:a> <a:b> ( [ <a:b> ( 1 ) ] ) .", 2));
        assert!(!deeper_than("<a:a> <a:b> ( [ <a:b> ( 1 ) ] ) .", 3));
        // Siblings do not nest.
        assert!(!deeper_than("<a:a> <a:b> ( 1 ) , ( 2 ) , [ ] , ( 3 ) .", 1));
        // Brackets in a comment, a string, a long string, an IRI and an
        // escaped local name are text, not structure.
        for inert in [
            "# ((((\n<a:a> <a:b> ( 1 ) .",
            "<a:a> <a:b> \"((((\" , '[[[[' , ( \"\\\"((\" ) .",
            "<a:a> <a:b> \"\"\"((\n\"(\"\"((\\\"\"\"((\"\"\" , ( '''[['[''' ) .",
            "<a:((((> <a:[[> ( <a:(> ) .",
            "@prefix e: <a:> . e:a\\(\\(\\( e:b ( e:c\\( ) .",
            "<a:a> <a:b> \"\" , ( '' ) .",
        ] {
            assert!(deeper_than(inert, 0), "{inert}");
            assert!(!deeper_than(inert, 1), "{inert}");
        }
        // Reified triples and annotation blocks nest like brackets do.
        assert!(deeper_than(
            "<< << <a:a> <a:b> <a:c> >> <a:b> <a:c> >> <a:b> 1 .",
            1
        ));
        assert!(!deeper_than(
            "<< <a:a> <a:b> <a:c> >> <a:b> << <a:a> <a:b> 2 >> .",
            1
        ));
        assert!(deeper_than(
            "<a:a> <a:b> 1 {| <a:b> 2 {| <a:b> 3 |} |} .",
            1
        ));
    }
}
