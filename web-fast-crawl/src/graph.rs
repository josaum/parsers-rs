//! Interned RDF terms and compact bidirectional adjacency indexes.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use thiserror::Error;

/// Dense term identifier; valid only within its containing graph.
pub type TermId = u32;
/// A resource or a typed/language-tagged RDF literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Term {
    /// An absolute IRI.
    Iri {
        /// IRI value.
        value: String,
    },
    /// A document-scoped blank node label, without `_:`.
    Blank {
        /// Scoped label.
        value: String,
    },
    /// A literal retaining its RDF datatype and optional language.
    Literal {
        /// Lexical value.
        value: String,
        /// Datatype IRI.
        datatype: String,
        /// Language tag.
        language: Option<String>,
    },
}
impl Term {
    /// Construct an IRI term.
    pub fn iri(value: impl Into<String>) -> Self {
        Self::Iri {
            value: value.into(),
        }
    }
    /// Construct a plain string literal.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Literal {
            value: value.into(),
            datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
            language: None,
        }
    }
    fn bytes(&self) -> usize {
        match self {
            Self::Iri { value } | Self::Blank { value } => value.len(),
            Self::Literal {
                value,
                datatype,
                language,
            } => value
                .len()
                .saturating_add(datatype.len())
                .saturating_add(language.as_ref().map_or(0, String::len)),
        }
    }
}
/// One deduplicated assertion with named-graph and source-page provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// Subject term.
    pub subject: TermId,
    /// Predicate IRI term.
    pub predicate: TermId,
    /// Object term.
    pub object: TermId,
    /// Original JSON-LD graph, when named.
    pub graph: Option<TermId>,
    /// Source page IRI; kept even when equivalent assertions occur elsewhere.
    pub source: TermId,
}
/// Limits on graph materialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphLimits {
    /// Maximum interned terms.
    pub max_terms: usize,
    /// Maximum distinct assertions, including provenance.
    pub max_edges: usize,
    /// Maximum combined UTF-8 bytes in interned terms.
    pub max_term_bytes: usize,
}
impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            max_terms: 1_000_000,
            max_edges: 2_000_000,
            max_term_bytes: 128 * 1024 * 1024,
        }
    }
}
/// Graph construction or query error.
#[derive(Debug, Error)]
pub enum GraphError {
    /// A configured resource limit was reached.
    #[error("graph resource limit: {0}")]
    Limit(&'static str),
    /// Invalid graph statement or algorithm options.
    #[error("invalid graph operation: {0}")]
    Invalid(&'static str),
}
/// Incremental term interning and assertion deduplication.
pub struct GraphBuilder {
    limits: GraphLimits,
    terms: Vec<Arc<Term>>,
    lookup: HashMap<Arc<Term>, TermId>,
    edges: Vec<Edge>,
    seen: HashSet<Edge>,
    bytes: usize,
}
impl GraphBuilder {
    /// Create an empty graph builder.
    pub fn new(limits: GraphLimits) -> Self {
        Self {
            limits,
            terms: Vec::new(),
            lookup: HashMap::new(),
            edges: Vec::new(),
            seen: HashSet::new(),
            bytes: 0,
        }
    }
    fn intern(&mut self, mut term: Term) -> Result<TermId, GraphError> {
        if let Term::Literal {
            language: Some(language),
            ..
        } = &mut term
        {
            language.make_ascii_lowercase();
        }
        if let Some(&id) = self.lookup.get(&term) {
            return Ok(id);
        }
        match &term {
            Term::Iri { value } => {
                oxrdf::NamedNodeRef::new(value).map_err(|_| GraphError::Invalid("IRI"))?;
            }
            Term::Blank { value } => {
                oxrdf::BlankNodeRef::new(value)
                    .map_err(|_| GraphError::Invalid("blank node label"))?;
            }
            Term::Literal {
                datatype, language, ..
            } => {
                oxrdf::NamedNodeRef::new(datatype)
                    .map_err(|_| GraphError::Invalid("datatype IRI"))?;
                if let Some(language) = language {
                    if datatype != "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" {
                        return Err(GraphError::Invalid("language literal datatype"));
                    }
                    oxrdf::Literal::new_language_tagged_literal("", language)
                        .map_err(|_| GraphError::Invalid("language tag"))?;
                }
            }
        }
        if self.terms.len() >= self.limits.max_terms {
            return Err(GraphError::Limit("terms"));
        }
        let id = u32::try_from(self.terms.len()).map_err(|_| GraphError::Limit("term IDs"))?;
        let bytes = self
            .bytes
            .checked_add(term.bytes())
            .ok_or(GraphError::Limit("term bytes"))?;
        if bytes > self.limits.max_term_bytes {
            return Err(GraphError::Limit("term bytes"));
        }
        let term = Arc::new(term);
        self.lookup.insert(term.clone(), id);
        self.terms.push(term);
        self.bytes = bytes;
        Ok(id)
    }
    /// Insert an assertion; return false for an exact duplicate.
    /// Resource terms already interned before a limit failure may remain unused.
    pub fn insert(
        &mut self,
        subject: Term,
        predicate: Term,
        object: Term,
        graph: Option<Term>,
        source: Term,
    ) -> Result<bool, GraphError> {
        if matches!(subject, Term::Literal { .. })
            || !matches!(predicate, Term::Iri { .. })
            || !matches!(source, Term::Iri { .. })
            || graph
                .as_ref()
                .is_some_and(|term| matches!(term, Term::Literal { .. }))
        {
            return Err(GraphError::Invalid("RDF term position"));
        }
        let edge = Edge {
            subject: self.intern(subject)?,
            predicate: self.intern(predicate)?,
            object: self.intern(object)?,
            graph: graph.map(|term| self.intern(term)).transpose()?,
            source: self.intern(source)?,
        };
        if self.seen.contains(&edge) {
            return Ok(false);
        }
        if self.edges.len() >= self.limits.max_edges || self.edges.len() >= u32::MAX as usize {
            return Err(GraphError::Limit("edges"));
        }
        self.seen.insert(edge);
        self.edges.push(edge);
        Ok(true)
    }
    /// Finish in O(terms + edges), constructing outgoing, incoming and predicate CSR indexes.
    pub fn finish(self) -> Graph {
        let outgoing = Adjacency::new(self.terms.len(), &self.edges, |edge| edge.subject);
        let incoming = Adjacency::new(self.terms.len(), &self.edges, |edge| edge.object);
        let predicates = Adjacency::new(self.terms.len(), &self.edges, |edge| edge.predicate);
        Graph {
            terms: self.terms,
            edges: self.edges,
            lookup: self.lookup,
            outgoing,
            incoming,
            predicates,
        }
    }
}

#[derive(Debug)]
struct Adjacency {
    offsets: Vec<usize>,
    edges: Vec<u32>,
}
impl Adjacency {
    fn new(nodes: usize, edges: &[Edge], key: impl Fn(&Edge) -> TermId) -> Self {
        let mut offsets = vec![0; nodes + 1];
        for edge in edges {
            offsets[key(edge) as usize + 1] += 1;
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets.clone();
        let mut ids = vec![0; edges.len()];
        for (i, edge) in edges.iter().enumerate() {
            let node = key(edge) as usize;
            ids[cursor[node]] = i as u32;
            cursor[node] += 1;
        }
        Self {
            offsets,
            edges: ids,
        }
    }
    fn get(&self, id: TermId) -> &[u32] {
        let i = id as usize;
        if i >= self.offsets.len().saturating_sub(1) {
            return &[];
        }
        &self.edges[self.offsets[i]..self.offsets[i + 1]]
    }
}
/// Immutable graph; serialization writes the term dictionary and compact edges only.
#[derive(Debug, Serialize)]
pub struct Graph {
    terms: Vec<Arc<Term>>,
    edges: Vec<Edge>,
    #[serde(skip)]
    lookup: HashMap<Arc<Term>, TermId>,
    #[serde(skip)]
    outgoing: Adjacency,
    #[serde(skip)]
    incoming: Adjacency,
    #[serde(skip)]
    predicates: Adjacency,
}
impl Graph {
    /// Resolve an interned ID.
    pub fn term(&self, id: TermId) -> Option<&Term> {
        self.terms.get(id as usize).map(AsRef::as_ref)
    }
    /// Find a term without scanning the dictionary.
    pub fn find(&self, term: &Term) -> Option<TermId> {
        self.lookup.get(term).copied()
    }
    /// Number of interned terms.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }
    /// Assertions in deterministic insertion order.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }
    /// Assertions with this subject, in O(out-degree).
    pub fn outgoing(&self, id: TermId) -> impl Iterator<Item = &Edge> {
        self.outgoing
            .get(id)
            .iter()
            .map(|&i| &self.edges[i as usize])
    }
    /// Assertions with this object, in O(in-degree).
    pub fn incoming(&self, id: TermId) -> impl Iterator<Item = &Edge> {
        self.incoming
            .get(id)
            .iter()
            .map(|&i| &self.edges[i as usize])
    }
    /// Assertions with this predicate, without a graph-wide scan.
    pub fn predicate(&self, id: TermId) -> impl Iterator<Item = &Edge> {
        self.predicates
            .get(id)
            .iter()
            .map(|&i| &self.edges[i as usize])
    }
    /// Match an RDF triple pattern by scanning the smallest available subject,
    /// predicate, or object adjacency list. No temporary result collection is built.
    pub fn matching(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
    ) -> impl Iterator<Item = &Edge> {
        let mut candidates: Option<&[u32]> = None;
        for ids in [
            subject.map(|id| self.outgoing.get(id)),
            predicate.map(|id| self.predicates.get(id)),
            object.map(|id| self.incoming.get(id)),
        ]
        .into_iter()
        .flatten()
        {
            if candidates.is_none_or(|current| ids.len() < current.len()) {
                candidates = Some(ids);
            }
        }
        (0..candidates.map_or(self.edges.len(), <[u32]>::len))
            .map(move |i| &self.edges[candidates.map_or(i, |ids| ids[i] as usize)])
            .filter(move |edge| {
                subject.is_none_or(|id| edge.subject == id)
                    && predicate.is_none_or(|id| edge.predicate == id)
                    && object.is_none_or(|id| edge.object == id)
            })
    }
    /// Breadth-first reachable resources, including the start, with depth and output limits.
    pub fn reachable(
        &self,
        start: TermId,
        predicate: Option<TermId>,
        max_depth: usize,
        max_nodes: usize,
    ) -> Vec<TermId> {
        if self.term(start).is_none() || max_nodes == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::from([start]);
        let mut queue = VecDeque::from([(start, 0)]);
        let mut result = Vec::new();
        while let Some((node, depth)) = queue.pop_front() {
            result.push(node);
            if depth >= max_depth {
                continue;
            }
            for edge in self.outgoing(node) {
                if predicate.is_some_and(|p| p != edge.predicate)
                    || matches!(self.term(edge.object), Some(Term::Literal { .. }))
                {
                    continue;
                }
                if seen.len() < max_nodes && seen.insert(edge.object) {
                    queue.push_back((edge.object, depth + 1));
                }
            }
        }
        result
    }
    /// PageRank over distinct resource-to-resource pairs for one predicate.
    /// Duplicate provenance does not multiply link weight. Only participating
    /// nodes are ranked. Results are ordered by term ID; scores sum to one.
    pub fn page_rank(
        &self,
        predicate: TermId,
        damping: f64,
        iterations: usize,
        tolerance: f64,
    ) -> Result<Vec<(TermId, f64)>, GraphError> {
        if !damping.is_finite()
            || !(0.0..1.0).contains(&damping)
            || !tolerance.is_finite()
            || tolerance < 0.0
            || iterations == 0
        {
            return Err(GraphError::Invalid("PageRank parameters"));
        }
        let mut pairs = self
            .predicate(predicate)
            .filter(|e| !matches!(self.term(e.object), Some(Term::Literal { .. })))
            .map(|e| (e.subject, e.object))
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        pairs.dedup();
        let mut nodes = pairs.iter().flat_map(|&(s, o)| [s, o]).collect::<Vec<_>>();
        nodes.sort_unstable();
        nodes.dedup();
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        let indices: HashMap<_, _> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let pairs = pairs
            .into_iter()
            .map(|(s, o)| (indices[&s], indices[&o]))
            .collect::<Vec<_>>();
        let mut degree = vec![0; nodes.len()];
        for &(s, _) in &pairs {
            degree[s] += 1;
        }
        let n = nodes.len() as f64;
        let mut scores = vec![1.0 / n; nodes.len()];
        let mut next = vec![0.0; nodes.len()];
        for _ in 0..iterations {
            let dangling: f64 = scores
                .iter()
                .zip(&degree)
                .filter(|(_, d)| **d == 0)
                .map(|(s, _)| s)
                .sum();
            next.fill((1.0 - damping + damping * dangling) / n);
            for &(s, o) in &pairs {
                next[o] += damping * scores[s] / degree[s] as f64;
            }
            let delta: f64 = scores.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
            std::mem::swap(&mut scores, &mut next);
            if delta <= tolerance {
                break;
            }
        }
        Ok(nodes.into_iter().zip(scores).collect())
    }
}
