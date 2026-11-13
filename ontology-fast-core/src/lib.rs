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
use std::hash::{BuildHasher, Hash, RandomState};

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
/// The Turtle parser keeps a few hundred bytes of heap state for every
/// `[ ... ]`, `( ... )`, `<< ... >>` or `{| ... |}` it is inside, so a megabyte
/// of opening brackets would otherwise cost hundreds of megabytes before a
/// single statement is read. Ten thousand levels is far beyond any authored
/// ontology and holds that state to a few megabytes. A document that nests
/// deeper is refused with [`OntologyError::ResourceLimit`] naming
/// `nesting_depth`, by every entry point, before it is parsed.
///
/// What this bounds is the parser's heap: its state for open brackets is a
/// list, not recursion, so `[`, `(`, `<<` and `{|` cost no call stack however
/// deep they go. It does not bound the call stack. The one construct that
/// costs stack per level is the RDF 1.2 triple term `<<( ... )>>`, whose
/// value is a term that owns a term; that has its own, much lower limit,
/// [`MAX_TRIPLE_TERM_DEPTH`]. Nor does it bound how many brackets a document
/// has in total, or what its statements cost: those are the source, statement
/// and term budgets.
///
/// The guard is a scan of the bytes, not a parse, and it runs first. It
/// counts what the parser would count in a well-formed document, so no valid
/// document within the limit is refused. A document that is malformed before
/// its brackets (an unterminated string followed by ten thousand `(`, say)
/// can be refused for `nesting_depth` where the parser alone would have
/// reported [`OntologyError::Syntax`]: it is refused either way, but the code
/// may differ.
pub const MAX_NESTING_DEPTH: usize = 10_000;

/// Deepest nesting of RDF 1.2 triple terms, `<<( ... )>>`, in one document.
///
/// A triple term is a term that owns another statement, so a triple term
/// inside a triple term is a chain of boxes. The parser builds that chain,
/// and the chain is taken apart again, by recursion: every level costs call
/// stack, where every other bracket costs only heap (see
/// [`MAX_NESTING_DEPTH`], which counts each `<<(` as two brackets and so
/// admitted five thousand of them). Measured on macOS/arm64 with `oxttl`
/// 0.2.5, a level costs about 720 bytes of stack in an unoptimized build and
/// about 330 in an optimized one, the same for every entry point; five
/// thousand levels overflow a 1 MiB thread and one thousand a 256 KiB one.
///
/// Thirty-two levels hold the stack this nesting can add to about 23 KiB
/// unoptimized (11 KiB optimized), under a tenth of a 256 KiB worker thread,
/// and are far beyond authored data, which nests a triple term a few levels
/// at most (a statement about a statement about a statement is three). A
/// document that
/// nests deeper is refused with [`OntologyError::ResourceLimit`] naming
/// `triple_term_depth`, by every entry point, before it is parsed.
///
/// The limit bounds only what triple-term nesting adds. It is not a bound on
/// the parser's whole stack: with no nesting at all an unoptimized build of
/// the parser already needs about 150 KiB (under 16 KiB optimized), so an
/// unoptimized build cannot run on a thread much smaller than 256 KiB at any
/// depth. Reified
/// triples `<< ... >>` and annotation blocks `{| ... |}` do not recurse and
/// are held by [`MAX_NESTING_DEPTH`] alone.
///
/// Like [`MAX_NESTING_DEPTH`], the guard is the scan of the bytes that runs
/// first: it counts the `<<(` tokens open at once, which is at least the
/// depth of any term the parser builds. A [`parse_turtle`] or
/// [`parse_turtle_terms`] call still refuses every triple term within the
/// limit with [`OntologyError::TripleTerm`]; only [`declared_prefixes`] reads
/// such a document.
pub const MAX_TRIPLE_TERM_DEPTH: usize = 32;

/// Least text one document's terms may expand to, whatever its size: 256 MiB.
///
/// See [`TERM_BYTES_PER_SOURCE_BYTE`] for the budget this is the floor of.
/// One million collection items, the most the default statement budget
/// admits, expand to about 230 MiB, so the floor leaves the statement budget
/// to refuse that document as it always has.
pub const TERM_BYTES_FLOOR: usize = 256 * 1024 * 1024;

/// Expanded term bytes allowed per source byte, above [`TERM_BYTES_FLOOR`].
///
/// A prefixed name or a relative IRI is a few source bytes that the parser
/// expands to the whole namespace or base, and every statement owns its copy.
/// A megabyte-long namespace used by a few hundred thousand three-byte names
/// therefore asks for hundreds of gigabytes of copying from a few megabytes of
/// source. So the text of every term the parser emits is counted: subject,
/// predicate and object of each statement, including a literal's datatype IRI
/// and language tag and the terms inside an RDF 1.2 triple term, and
/// including statements that repeat an earlier one. A document may expand to
///
/// ```text
/// max(TERM_BYTES_FLOOR, source bytes * TERM_BYTES_PER_SOURCE_BYTE)
/// ```
///
/// bytes; the statement that passes it is refused with
/// [`OntologyError::ResourceLimit`] naming `term_bytes`, by every entry point.
/// [`parse_turtle`] holds the text it renders to the same budget, since a
/// long prefix name expands a short IRI just as a long namespace expands a
/// short name. Authored ontologies expand five to ten times (the 50 MB
/// benchmark document: about six), and a document under 8 MiB is held by the
/// floor alone.
pub const TERM_BYTES_PER_SOURCE_BYTE: usize = 32;

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
/// [`OntologyError::ResourceLimit`] when a budget, [`MAX_NESTING_DEPTH`],
/// [`MAX_TRIPLE_TERM_DEPTH`] or the expanded-text allowance
/// ([`TERM_BYTES_PER_SOURCE_BYTE`]) is exceeded,
/// [`OntologyError::NamedGraph`] when a quad names a graph, and
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
    let term_budget = term_byte_budget(source.len());
    let rendered = terms.render(&abbreviator, term_budget)?;
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
    // Every row owns its text, so the rendered text is held to the budget of
    // the expanded terms before it is copied out.
    let text_of = |number: &u32| rendered[*number as usize].len();
    let mut row_bytes = 0_usize;
    for row in &rows {
        row_bytes = row_bytes.saturating_add(row.iter().map(text_of).sum());
        enforce_term_budget(row_bytes, term_budget)?;
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
/// [`OntologyError::ResourceLimit`] when a budget, [`MAX_NESTING_DEPTH`],
/// [`MAX_TRIPLE_TERM_DEPTH`] or the expanded-text allowance
/// ([`TERM_BYTES_PER_SOURCE_BYTE`]) is exceeded,
/// [`OntologyError::NamedGraph`] when a quad names a graph,
/// [`OntologyError::TripleTerm`] for an RDF 1.2 triple term, and
/// [`OntologyError::BaseDirection`] for a literal carrying a base direction.
pub fn parse_turtle_terms(
    source: &[u8],
    limits: OntologyLimits,
) -> Result<Vec<TripleTerms>, OntologyError> {
    // Deduplication compares six numbers per statement instead of the
    // statement's text: each distinct string is numbered once, and the one
    // copy kept of it is the one in the row that first used it.
    let mut triples: Vec<TripleTerms> = Vec::new();
    let mut strings: RowStrings = RowStrings::default();
    let mut seen: HashSet<[u32; 6]> = HashSet::new();
    read_default_graph(source, limits, |quad, blank_labels| {
        let subject = match &quad.subject {
            NamedOrBlankNode::NamedNode(node) => node.as_str(),
            NamedOrBlankNode::BlankNode(node) => blank_labels.label(node.as_str()),
        }
        .to_owned();
        let object = object_view(&quad.object, blank_labels)?;
        // The row goes in first so that its strings can be numbered where
        // they will stay. A string seen for the first time gives the row a
        // key never seen before, so a row taken out again was never the home
        // of any string.
        triples.push(TripleTerms {
            subject,
            predicate: quad.predicate.as_str().to_owned(),
            object,
        });
        let key = strings.key_of_last(&triples)?;
        if seen.insert(key) {
            enforce_statement_budget(seen.len(), limits)?;
        } else {
            triples.pop();
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
/// `limits.max_source_bytes`, [`MAX_NESTING_DEPTH`],
/// [`MAX_TRIPLE_TERM_DEPTH`] or the expanded-text
/// allowance ([`TERM_BYTES_PER_SOURCE_BYTE`]), and [`OntologyError::Syntax`]
/// when the document does not parse: a table read from half a document would
/// be a guess.
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
    match first_nesting_past(source, MAX_NESTING_DEPTH, MAX_TRIPLE_TERM_DEPTH) {
        Some(Nesting::Brackets) => {
            return Err(OntologyError::ResourceLimit {
                resource: "nesting_depth",
                limit: MAX_NESTING_DEPTH,
                observed: MAX_NESTING_DEPTH + 1,
            });
        }
        Some(Nesting::TripleTerms) => {
            return Err(OntologyError::ResourceLimit {
                resource: "triple_term_depth",
                limit: MAX_TRIPLE_TERM_DEPTH,
                observed: MAX_TRIPLE_TERM_DEPTH + 1,
            });
        }
        None => {}
    }
    let mut blank_labels = BlankLabels::for_document(source);
    let term_budget = term_byte_budget(source.len());
    let mut term_bytes = 0_usize;
    let mut parser = TurtleParser::new().for_slice(source);
    // `by_ref`: the parser is read again after the last triple, for the
    // prefixes it saw.
    for triple in parser.by_ref() {
        let triple = triple.map_err(|error| OntologyError::Syntax(error.to_string()))?;
        // Counted before the statement is looked at: a repeated statement is
        // retained nowhere, but the parser has still made its copies.
        term_bytes = term_bytes.saturating_add(triple_term_bytes(&triple));
        enforce_term_budget(term_bytes, term_budget)?;
        // Turtle has no graph syntax, so every statement is in the default
        // graph; `OntologyError::NamedGraph` stays for a future quad format.
        let quad = triple.in_graph(GraphName::DefaultGraph);
        visit(&quad, &mut blank_labels)?;
    }
    Ok(parser
        .prefixes()
        .map(|(prefix, namespace)| (prefix.to_owned(), namespace.to_owned()))
        .collect())
}

/// A nesting limit of [`first_nesting_past`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Nesting {
    /// Brackets of any kind open at once.
    Brackets,
    /// Triple terms, `<<(`, open at once.
    TripleTerms,
}

/// Whether `source` opens more than `limit` brackets inside one another.
#[cfg(test)]
fn nests_deeper_than(source: &[u8], limit: usize) -> bool {
    first_nesting_past(source, limit, usize::MAX).is_some()
}

/// The first nesting limit `source` passes, reading from its start.
///
/// `limit` is for brackets of any kind open at once, `triple_term_limit` for
/// `<<(` open at once. The tokenizer reads `<<(` and `)>>` as one token each,
/// never with white space inside, and accepts `)>>` only where it closes a
/// `<<(`; each still counts as two brackets toward `limit`.
///
/// A scan of the bytes, not a parse: it steps over what cannot hold a bracket
/// (comments, quoted strings, IRIs and escaped characters) and counts `(`,
/// `[`, `{` and `<<` against `)`, `]`, `}` and `>>`. It follows the Turtle
/// tokenizer for as long as the document is well formed, and the parser stops
/// at the first token that is not, so the count is the parser's own depth for
/// every byte the parser would go on to read.
fn first_nesting_past(source: &[u8], limit: usize, triple_term_limit: usize) -> Option<Nesting> {
    let mut depth = 0_usize;
    let mut triple_terms = 0_usize;
    let mut at = 0_usize;
    while let Some(byte) = source.get(at).copied() {
        at += 1;
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return Some(Nesting::Brackets);
                }
            }
            b')' => {
                depth = depth.saturating_sub(1);
                // `)>>`: the `>>` is counted when the scan reaches it.
                if source.get(at) == Some(&b'>') && source.get(at + 1) == Some(&b'>') {
                    triple_terms = triple_terms.saturating_sub(1);
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
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
                    return Some(Nesting::Brackets);
                }
                // `<<(`: the `(` is counted when the scan reaches it.
                if source.get(at) == Some(&b'(') {
                    triple_terms += 1;
                    if triple_terms > triple_term_limit {
                        return Some(Nesting::TripleTerms);
                    }
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
    None
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

/// The expanded term bytes a source of `source_bytes` bytes may ask for.
///
/// See [`TERM_BYTES_PER_SOURCE_BYTE`].
fn term_byte_budget(source_bytes: usize) -> usize {
    TERM_BYTES_FLOOR.max(source_bytes.saturating_mul(TERM_BYTES_PER_SOURCE_BYTE))
}

/// Refuse once `observed` expanded term bytes pass `budget`.
fn enforce_term_budget(observed: usize, budget: usize) -> Result<(), OntologyError> {
    if observed > budget {
        return Err(OntologyError::ResourceLimit {
            resource: "term_bytes",
            limit: budget,
            observed,
        });
    }
    Ok(())
}

/// The text bytes of every term of `triple`, as the parser expanded them.
///
/// A loop, not recursion: a triple term is a chain through the object
/// position, and the chain is followed without the call stack.
fn triple_term_bytes(triple: &oxrdf::Triple) -> usize {
    let mut bytes = 0_usize;
    let mut next = Some(triple);
    while let Some(triple) = next.take() {
        let subject = match &triple.subject {
            NamedOrBlankNode::NamedNode(node) => node.as_str().len(),
            NamedOrBlankNode::BlankNode(node) => node.as_str().len(),
        };
        let object = match &triple.object {
            Term::NamedNode(node) => node.as_str().len(),
            Term::BlankNode(node) => node.as_str().len(),
            Term::Literal(literal) => literal
                .value()
                .len()
                .saturating_add(literal.datatype().as_str().len())
                .saturating_add(literal.language().map_or(0, str::len)),
            Term::Triple(inner) => {
                next = Some(inner);
                0
            }
        };
        bytes = bytes
            .saturating_add(subject)
            .saturating_add(triple.predicate.as_str().len())
            .saturating_add(object);
    }
    bytes
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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TermKind {
    /// A full IRI in subject or object position, abbreviated at render time.
    Iri = 0,
    /// Text that renders as it stands: a literal's lexical form or a blank label.
    Verbatim = 1,
    /// A full IRI in predicate position: `rdf:type` renders as `a`.
    Predicate = 2,
}

/// Numbers for distinct keys, by first appearance, hashing each key once.
///
/// A map keyed on the strings themselves hashes a new string twice (the
/// lookup, then the insert) and every string again whenever the table grows.
/// This table is keyed on the 64-bit hash instead and leaves the text with
/// the caller: a lookup hashes its key once, then asks the caller whether a
/// candidate number really is that key.
#[derive(Debug, Default)]
struct Numbers<S = RandomState> {
    state: S,
    /// The latest number assigned under each hash.
    latest: HashMap<u64, u32>,
    /// `earlier[n]` is an earlier number with the hash of `n`, or `NO_NUMBER`.
    earlier: Vec<u32>,
}

/// "No number": the end of a chain in [`Numbers`]. Never assigned.
const NO_NUMBER: u32 = u32::MAX;

impl<S: BuildHasher> Numbers<S> {
    /// The number of `key`, and whether this call assigned it.
    ///
    /// `is_key(n)` answers whether number `n` was assigned to this same key.
    /// Numbers start at zero and are assigned in order, so a caller that
    /// keeps one record per number appends one when the answer is "new".
    fn number(
        &mut self,
        key: impl Hash,
        is_key: impl Fn(u32) -> bool,
    ) -> Result<(u32, bool), OntologyError> {
        let hash = self.state.hash_one(key);
        let latest = self.latest.get(&hash).copied().unwrap_or(NO_NUMBER);
        let mut candidate = latest;
        while candidate != NO_NUMBER {
            if is_key(candidate) {
                return Ok((candidate, false));
            }
            candidate = self
                .earlier
                .get(candidate as usize)
                .copied()
                .unwrap_or(NO_NUMBER);
        }
        let number = u32::try_from(self.earlier.len()).unwrap_or(NO_NUMBER);
        // Refuses `NO_NUMBER` itself, so the sentinel is never a number.
        next_number(number)?;
        self.latest.insert(hash, number);
        self.earlier.push(latest);
        Ok((number, true))
    }
}

/// The distinct terms of one document, numbered by first appearance.
#[derive(Debug, Default)]
struct StagedTerms {
    numbers: Numbers,
    /// The kind and the full text of each term, at the index of its number.
    kinds: Vec<TermKind>,
    texts: Vec<String>,
}

impl StagedTerms {
    /// The number of the term `text` of kind `kind`, assigning the next on first sight.
    fn number(&mut self, kind: TermKind, text: &str) -> Result<u32, OntologyError> {
        let (kinds, texts) = (&self.kinds, &self.texts);
        let (number, new) = self.numbers.number((kind, text), |number| {
            let number = number as usize;
            kinds.get(number) == Some(&kind) && texts.get(number).map(String::as_str) == Some(text)
        })?;
        if new {
            self.kinds.push(kind);
            self.texts.push(text.to_owned());
        }
        Ok(number)
    }

    /// The text form of every term, at the index of its number.
    ///
    /// Refuses once the rendered text passes `budget` bytes: a prefix name
    /// can be far longer than the namespace it stands for.
    fn render(
        self,
        abbreviator: &Abbreviator<'_>,
        budget: usize,
    ) -> Result<Vec<String>, OntologyError> {
        let mut bytes = 0_usize;
        self.kinds
            .into_iter()
            .zip(self.texts)
            .map(|(kind, text)| {
                let text = match kind {
                    TermKind::Verbatim => text,
                    TermKind::Predicate if text == RDF_TYPE => "a".to_owned(),
                    TermKind::Iri | TermKind::Predicate => abbreviator.abbreviate(&text),
                };
                bytes = bytes.saturating_add(text.len());
                enforce_term_budget(bytes, budget)?;
                Ok(text)
            })
            .collect()
    }
}

/// Numbers for the distinct strings of the term view's rows.
///
/// No string is copied to be numbered: each number records the row and the
/// part of it where its string was first seen.
#[derive(Debug, Default)]
struct RowStrings<S = RandomState> {
    numbers: Numbers<S>,
    /// Where the string of each number lives, at the index of the number.
    homes: Vec<(usize, RowPart)>,
}

/// One string-valued part of a [`TripleTerms`] row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowPart {
    Subject,
    Predicate,
    /// An IRI, a blank label or a literal's lexical form.
    Object,
    Datatype,
    Language,
}

impl RowPart {
    /// This part of `row`, when the row has it.
    fn of(self, row: &TripleTerms) -> Option<&str> {
        match (self, &row.object) {
            (Self::Subject, _) => Some(&row.subject),
            (Self::Predicate, _) => Some(&row.predicate),
            (
                Self::Object,
                TermView::Iri(text) | TermView::Blank(text) | TermView::Literal { value: text, .. },
            ) => Some(text),
            (Self::Datatype, TermView::Literal { datatype, .. }) => datatype.as_deref(),
            (Self::Language, TermView::Literal { language, .. }) => language.as_deref(),
            (Self::Datatype | Self::Language, _) => None,
        }
    }
}

impl<S: BuildHasher> RowStrings<S> {
    /// The deduplication key of the last row of `rows`: six numbers.
    ///
    /// Two rows have the same key exactly when they are the same statement.
    /// Zero is "absent"; a present string is its number plus one.
    fn key_of_last(&mut self, rows: &[TripleTerms]) -> Result<[u32; 6], OntologyError> {
        let Some((row, last)) = rows.len().checked_sub(1).zip(rows.last()) else {
            return Ok([0; 6]);
        };
        let kind = match &last.object {
            TermView::Iri(_) => 0,
            TermView::Blank(_) => 1,
            TermView::Literal { .. } => 2,
        };
        let mut number = |part: RowPart| -> Result<u32, OntologyError> {
            let Some(text) = part.of(last) else {
                return Ok(0);
            };
            let homes = &self.homes;
            let (number, new) = self.numbers.number(text, |number| {
                homes
                    .get(number as usize)
                    .and_then(|(home, part)| part.of(rows.get(*home)?))
                    == Some(text)
            })?;
            if new {
                self.homes.push((row, part));
            }
            next_number(number)
        };
        Ok([
            number(RowPart::Subject)?,
            number(RowPart::Predicate)?,
            kind,
            number(RowPart::Object)?,
            number(RowPart::Datatype)?,
            number(RowPart::Language)?,
        ])
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

    /// A `BuildHasher` that counts the bytes it is asked to hash.
    #[derive(Clone, Debug, Default)]
    struct CountingState(std::rc::Rc<std::cell::Cell<usize>>);

    struct CountingHasher {
        inner: std::collections::hash_map::DefaultHasher,
        bytes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl std::hash::Hasher for CountingHasher {
        fn finish(&self) -> u64 {
            self.inner.finish()
        }

        fn write(&mut self, bytes: &[u8]) {
            self.bytes.set(self.bytes.get() + bytes.len());
            self.inner.write(bytes);
        }
    }

    impl std::hash::BuildHasher for CountingState {
        type Hasher = CountingHasher;

        fn build_hasher(&self) -> CountingHasher {
            CountingHasher {
                inner: std::collections::hash_map::DefaultHasher::new(),
                bytes: std::rc::Rc::clone(&self.0),
            }
        }
    }

    #[test]
    fn numbering_a_string_hashes_it_once_per_lookup() {
        let state = CountingState::default();
        let mut numbers = Numbers {
            state: state.clone(),
            latest: HashMap::new(),
            earlier: Vec::new(),
        };
        let mut texts: Vec<String> = Vec::new();
        let (count, length) = (200, 1_000);
        for n in 0..count {
            let text = format!("{n:0length$}");
            let number = u32::try_from(n).expect("small");
            let first = numbers.number(text.as_str(), |known| texts[known as usize] == text);
            assert_eq!(first, Ok((number, true)));
            texts.push(text.clone());
            let again = numbers.number(text.as_str(), |known| texts[known as usize] == text);
            assert_eq!(again, Ok((number, false)));
        }
        let looked_up = 2 * count * length;
        let hashed = state.0.get();
        assert!(
            hashed <= looked_up + looked_up / 10,
            "{hashed} bytes hashed for {looked_up} bytes looked up"
        );
    }

    /// A `BuildHasher` under which every key has the same hash.
    #[derive(Clone, Copy, Debug, Default)]
    struct OneHash;

    impl std::hash::Hasher for OneHash {
        fn finish(&self) -> u64 {
            7
        }

        fn write(&mut self, _: &[u8]) {}
    }

    impl BuildHasher for OneHash {
        type Hasher = Self;

        fn build_hasher(&self) -> Self {
            Self
        }
    }

    #[test]
    fn strings_that_share_a_hash_still_get_their_own_numbers() {
        let mut strings = RowStrings {
            numbers: Numbers {
                state: OneHash,
                latest: HashMap::new(),
                earlier: Vec::new(),
            },
            homes: Vec::new(),
        };
        let row = |subject: &str, object: TermView| TripleTerms {
            subject: subject.to_owned(),
            predicate: "a:p".to_owned(),
            object,
        };
        let literal = |value: &str, language: Option<&str>| TermView::Literal {
            value: value.to_owned(),
            datatype: None,
            language: language.map(str::to_owned),
        };
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        for next in [
            row("a:s", TermView::Iri("a:o".to_owned())),
            row("a:p", TermView::Iri("a:p".to_owned())),
            row("a:s", TermView::Blank("a:o".to_owned())),
            row("a:s", literal("a:o", None)),
            row("a:s", literal("a:o", Some("en"))),
            row("a:s", TermView::Iri("a:o".to_owned())),
        ] {
            rows.push(next);
            keys.push(strings.key_of_last(&rows).expect("numbered"));
        }
        // Subject, predicate, kind, object text, datatype, language; a
        // present string is its number plus one.
        assert_eq!(
            keys,
            [
                [1, 2, 0, 3, 0, 0],
                [2, 2, 0, 2, 0, 0],
                [1, 2, 1, 3, 0, 0],
                [1, 2, 2, 3, 0, 0],
                [1, 2, 2, 3, 0, 4],
                [1, 2, 0, 3, 0, 0],
            ]
        );
    }

    #[test]
    fn the_term_budget_scales_with_the_source_above_its_floor() {
        let floor_covers = TERM_BYTES_FLOOR / TERM_BYTES_PER_SOURCE_BYTE;
        assert_eq!(term_byte_budget(0), TERM_BYTES_FLOOR);
        assert_eq!(term_byte_budget(floor_covers), TERM_BYTES_FLOOR);
        assert_eq!(
            term_byte_budget(floor_covers + 1),
            TERM_BYTES_FLOOR + TERM_BYTES_PER_SOURCE_BYTE
        );
        assert_eq!(term_byte_budget(usize::MAX), usize::MAX);
        assert_eq!(enforce_term_budget(7, 7), Ok(()));
        assert_eq!(
            enforce_term_budget(8, 7),
            Err(OntologyError::ResourceLimit {
                resource: "term_bytes",
                limit: 7,
                observed: 8,
            })
        );
    }

    #[test]
    fn term_bytes_count_every_part_of_a_statement() {
        let count = |text: &str| -> usize {
            TurtleParser::new()
                .for_slice(text.as_bytes())
                .map(|triple| triple_term_bytes(&triple.expect("parses")))
                .sum()
        };
        // 3 + 3 + 3 bytes of IRI.
        assert_eq!(count("<a:s> <a:p> <a:o> ."), 9);
        // Lexical form, language tag, and the `rdf:langString` datatype IRI.
        assert_eq!(count("<a:s> <a:p> 'xy'@en ."), 6 + 2 + 2 + 53);
        // Lexical form and datatype IRI.
        assert_eq!(count("<a:s> <a:p> 'xy'^^<a:dt> ."), 6 + 2 + 4);
        // A triple term's own three terms.
        assert_eq!(count("<a:s> <a:p> <<( <a:x> <a:y> <a:z> )>> ."), 6 + 9);
        // Every level of a nested one.
        assert_eq!(
            count("<a:s> <a:p> <<( <a:x> <a:y> <<( <a:x> <a:y> 'z' )>> )>> ."),
            6 + 6 + 6 + 1 + 39
        );
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
        // Triple terms have their own count: `<<(` open at once. Each is two
        // brackets, so the bracket limit is kept out of the way here.
        let past = |text: &str, limit| first_nesting_past(text.as_bytes(), 100, limit);
        let two = "<a:a> <a:b> <<( <a:a> <a:b> <<( <a:a> <a:b> 1 )>> )>> .";
        assert_eq!(past(two, 1), Some(Nesting::TripleTerms));
        assert_eq!(past(two, 2), None);
        // Siblings do not nest, and a reified triple or a collection is not a
        // triple term.
        let siblings = "<a:a> <a:b> <<( <a:a> <a:b> 1 )>> , <<( <a:a> <a:b> 2 )>> , \
                        << <a:a> <a:b> ( 3 ) >> , ( ( 4 ) ) .";
        assert_eq!(past(siblings, 1), None);
        assert_eq!(past(siblings, 0), Some(Nesting::TripleTerms));
        // Whichever limit is passed first is the one reported.
        assert_eq!(
            first_nesting_past(two.as_bytes(), 1, 1),
            Some(Nesting::Brackets)
        );
        assert_eq!(
            first_nesting_past(two.as_bytes(), 3, 1),
            Some(Nesting::TripleTerms)
        );
    }
}
