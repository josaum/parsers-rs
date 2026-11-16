//! Offline JSON-LD to RDF and HTML evidence projection.
use crate::graph::{GraphBuilder, GraphError, Term};
use oxjsonld::{JsonLdParser, JsonLdRemoteDocument};
use oxrdf::{GraphName, NamedOrBlankNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use web_fast_core::WebDocument;

/// RDF type predicate.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// Predicate used for the crawl link graph.
pub const LINK: &str = "https://schema.org/relatedLink";
/// Canonical hints remain evidence, not identity merges.
pub const CANONICAL: &str = "urn:web-fast:canonical";
/// Observed HTTP redirect relation.
pub const REDIRECT: &str = "urn:web-fast:redirect";
/// Byte-identical response bodies, without claiming semantic identity.
pub const DUPLICATE: &str = "urn:web-fast:duplicateContent";

/// Limits and offline context policy for semantic projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticOptions {
    /// Maximum statements produced for one document.
    pub max_statements: usize,
    /// Maximum serialized JSON-LD bytes across scripts.
    pub max_jsonld_bytes: usize,
    /// Maximum bytes in one explicitly supplied context.
    pub max_context_bytes: usize,
    /// Maximum context loader calls per JSON-LD script, including nested contexts.
    pub max_context_loads: usize,
    /// Include semantic blocks and heading hierarchy with source paths.
    pub include_blocks: bool,
    /// Include anchor text, relation tokens and DOM path evidence per link.
    pub include_link_evidence: bool,
    /// Exact context IRI to trusted context bytes. No context is fetched over the network.
    pub contexts: Arc<BTreeMap<String, Vec<u8>>>,
}
impl Default for SemanticOptions {
    fn default() -> Self {
        Self {
            max_statements: 50_000,
            max_jsonld_bytes: 4 * 1024 * 1024,
            max_context_bytes: 2 * 1024 * 1024,
            max_context_loads: 16,
            include_blocks: true,
            include_link_evidence: true,
            contexts: Arc::new(BTreeMap::new()),
        }
    }
}
/// A full RDF assertion before dictionary interning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Statement {
    /// Subject resource.
    pub subject: Term,
    /// Predicate IRI.
    pub predicate: Term,
    /// Object resource or literal.
    pub object: Term,
    /// Original named graph, if present.
    pub graph: Option<Term>,
}
/// Semantic projection with bounded, payload-free diagnostics.
#[derive(Debug, Serialize)]
pub struct SemanticDocument {
    /// Page URL that supplied these assertions.
    pub source: String,
    /// Statements in extraction order.
    pub statements: Vec<Statement>,
    /// URL-valued JSON-LD relationships eligible for scoped discovery.
    pub link_candidates: Vec<String>,
    /// Rejected JSON-LD blocks; no remote context request is made implicitly.
    pub diagnostics: Vec<String>,
}
impl SemanticDocument {
    /// Add statements with source-page provenance to the graph.
    pub fn insert_into(self, graph: &mut GraphBuilder) -> Result<(), GraphError> {
        for statement in self.statements {
            graph.insert(
                statement.subject,
                statement.predicate,
                statement.object,
                statement.graph,
                Term::iri(&self.source),
            )?;
        }
        Ok(())
    }
}

struct Projection {
    document: SemanticDocument,
    max: usize,
}
impl Projection {
    fn url_term(&mut self, value: &str) -> Term {
        if oxrdf::NamedNodeRef::new(value).is_ok() {
            Term::iri(value)
        } else {
            let code = "non_rdf_url_preserved_as_text";
            if !self.document.diagnostics.iter().any(|item| item == code) {
                self.document.diagnostics.push(code.into());
            }
            Term::text(value)
        }
    }
    fn push(&mut self, subject: Term, predicate: &str, object: Term) -> Result<(), GraphError> {
        self.statement(Statement {
            subject,
            predicate: Term::iri(predicate),
            object,
            graph: None,
        })
    }
    fn statement(&mut self, statement: Statement) -> Result<(), GraphError> {
        if self.document.statements.len() >= self.max {
            return Err(GraphError::Limit("page statements"));
        }
        self.document.statements.push(statement);
        Ok(())
    }
}

/// Project a parsed page into RDF assertions, resolving JSON-LD with Oxigraph.
/// Inline contexts and the bundled Schema.org context work offline. Unregistered
/// remote contexts and unsupported RDF 1.2 terms reject their entire script.
pub fn extract_semantics(
    document: &WebDocument,
    source_url: &str,
    options: &SemanticOptions,
) -> Result<SemanticDocument, GraphError> {
    let source = url::Url::parse(source_url).map_err(|_| GraphError::Invalid("source URL"))?;
    let source_url = source.as_str();
    if options
        .contexts
        .values()
        .any(|bytes| bytes.len() > options.max_context_bytes)
    {
        return Err(GraphError::Limit("context bytes"));
    }
    let scope = format!(
        "{:x}",
        Sha256::digest(format!("{source_url}\0{}", document.source_sha256))
    );
    let page = Term::iri(source_url);
    let generated = |kind: &str, i: usize| Term::iri(format!("urn:web-fast:{scope}:{kind}:{i}"));
    let mut out = Projection {
        document: SemanticDocument {
            source: source_url.into(),
            statements: Vec::new(),
            link_candidates: Vec::new(),
            diagnostics: Vec::new(),
        },
        max: options.max_statements,
    };
    out.push(
        page.clone(),
        RDF_TYPE,
        Term::iri("https://schema.org/WebPage"),
    )?;
    out.push(
        page.clone(),
        "urn:web-fast:sourceSha256",
        Term::text(&document.source_sha256),
    )?;
    for (predicate, value) in [
        ("https://schema.org/name", &document.title),
        ("https://schema.org/description", &document.description),
        ("https://schema.org/inLanguage", &document.language),
    ] {
        if let Some(value) = value {
            out.push(page.clone(), predicate, Term::text(value))?;
        }
    }
    if let Some(url) = &document.canonical_url {
        let target = out.url_term(url);
        out.push(page.clone(), CANONICAL, target)?;
    }
    for (i, meta) in document.metadata.iter().enumerate() {
        let node = generated("meta", i);
        out.push(page.clone(), "urn:web-fast:metadata", node.clone())?;
        out.push(
            node.clone(),
            "urn:web-fast:metadataKind",
            Term::text(&meta.kind),
        )?;
        out.push(
            node.clone(),
            "urn:web-fast:metadataName",
            Term::text(&meta.name),
        )?;
        out.push(node, "https://schema.org/value", Term::text(&meta.content))?;
    }
    if options.include_blocks {
        let mut headings: Vec<(u8, Term)> = Vec::new();
        for block in &document.blocks {
            let node = generated("block", block.ordinal as usize);
            if let Some(level) = block.heading_level {
                while headings
                    .last()
                    .is_some_and(|(parent_level, _)| *parent_level >= level)
                {
                    headings.pop();
                }
            }
            let parent = headings.last().map_or(&page, |(_, node)| node);
            out.push(parent.clone(), "https://schema.org/hasPart", node.clone())?;
            out.push(
                node.clone(),
                RDF_TYPE,
                Term::iri("https://schema.org/WebPageElement"),
            )?;
            out.push(
                node.clone(),
                "https://schema.org/text",
                Term::text(&block.text),
            )?;
            out.push(
                node.clone(),
                "urn:web-fast:domPath",
                Term::text(&block.dom_path),
            )?;
            if let Some(level) = block.heading_level {
                out.push(
                    node.clone(),
                    "urn:web-fast:headingLevel",
                    Term::Literal {
                        value: level.to_string(),
                        datatype: "http://www.w3.org/2001/XMLSchema#integer".into(),
                        language: None,
                    },
                )?;
                headings.push((level, node));
            }
        }
    }
    for link in &document.links {
        if let Some(url) = &link.resolved_url {
            let target = out.url_term(url);
            let crawl_target = crate::normalize_url(url, usize::MAX)
                .map(|url| Term::iri(url.as_str()))
                .unwrap_or_else(|_| target.clone());
            if matches!(crawl_target, Term::Iri { .. }) {
                out.push(page.clone(), LINK, crawl_target)?;
            }
            if options.include_link_evidence {
                let node = generated("link", link.ordinal as usize);
                out.push(page.clone(), "urn:web-fast:linkEvidence", node.clone())?;
                out.push(node.clone(), "https://schema.org/url", target)?;
                out.push(
                    node.clone(),
                    "https://schema.org/text",
                    Term::text(&link.text),
                )?;
                out.push(
                    node.clone(),
                    "urn:web-fast:domPath",
                    Term::text(&link.dom_path),
                )?;
                if let Some(rel) = &link.rel {
                    out.push(node, "urn:web-fast:rel", Term::text(rel))?;
                }
            }
        }
    }
    let contexts = options.contexts.clone();
    let mut json_bytes = 0usize;
    for (script_index, value) in document.json_ld.iter().enumerate() {
        let mut writer = LimitedJson {
            bytes: Vec::new(),
            limit: options.max_jsonld_bytes.saturating_sub(json_bytes),
        };
        serde_json::to_writer(&mut writer, value)
            .map_err(|_| GraphError::Limit("JSON-LD bytes"))?;
        let bytes = writer.bytes;
        json_bytes = json_bytes
            .checked_add(bytes.len())
            .ok_or(GraphError::Limit("JSON-LD bytes"))?;
        if json_bytes > options.max_jsonld_bytes {
            return Err(GraphError::Limit("JSON-LD bytes"));
        }
        let contexts = contexts.clone();
        let calls = AtomicUsize::new(0);
        let max_calls = options.max_context_loads;
        let max_context_bytes = options.max_context_bytes;
        let parser = JsonLdParser::new()
            .with_base_iri(document.base_url.as_deref().unwrap_or(source_url))
            .map_err(|_| GraphError::Invalid("JSON-LD base"))?
            .for_slice(&bytes)
            .with_load_document_callback(move |url, _| {
                if calls.fetch_add(1, Ordering::Relaxed) >= max_calls {
                    return Err("context load limit".into());
                }
                let content = if let Some(bytes) = contexts.get(url) {
                    bytes.clone()
                } else if matches!(
                    url,
                    "http://schema.org"
                        | "http://schema.org/"
                        | "https://schema.org"
                        | "https://schema.org/"
                        | "https://schema.org/docs/jsonldcontext.jsonld"
                ) {
                    include_bytes!("../contexts/schema.org.jsonld").to_vec()
                } else {
                    return Err("unregistered offline context".into());
                };
                if content.len() > max_context_bytes {
                    return Err("context byte limit".into());
                }
                Ok(JsonLdRemoteDocument {
                    document_url: url.into(),
                    document: content,
                })
            });
        let mut blanks = HashMap::new();
        let blank_scope = format!("d{scope}s{script_index}");
        let mut statements = Vec::new();
        let mut invalid = false;
        for quad in parser {
            let Ok(quad) = quad else {
                invalid = true;
                break;
            };
            if out.document.statements.len() + statements.len() >= options.max_statements {
                return Err(GraphError::Limit("page statements"));
            }
            let subject = resource(quad.subject, &blank_scope, &mut blanks);
            let object = match quad.object {
                oxrdf::Term::NamedNode(node) => Term::iri(node.as_str()),
                oxrdf::Term::BlankNode(node) => blank(node.as_str(), &blank_scope, &mut blanks),
                oxrdf::Term::Literal(literal) if literal.direction().is_none() => Term::Literal {
                    value: literal.value().into(),
                    datatype: literal.datatype().as_str().into(),
                    language: literal.language().map(str::to_owned),
                },
                _ => {
                    invalid = true;
                    break;
                }
            };
            let graph = match quad.graph_name {
                GraphName::DefaultGraph => None,
                GraphName::NamedNode(node) => Some(Term::iri(node.as_str())),
                GraphName::BlankNode(node) => Some(blank(node.as_str(), &blank_scope, &mut blanks)),
            };
            statements.push(Statement {
                subject,
                predicate: Term::iri(quad.predicate.as_str()),
                object,
                graph,
            });
        }
        if invalid {
            out.document
                .diagnostics
                .push(format!("json_ld_rejected:{script_index}"));
        } else {
            for statement in statements {
                if let (Term::Iri { value: predicate }, Term::Iri { value: target }) =
                    (&statement.predicate, &statement.object)
                {
                    if matches!(
                        predicate.as_str(),
                        "https://schema.org/url"
                            | "http://schema.org/url"
                            | "https://schema.org/mainEntityOfPage"
                            | "http://schema.org/mainEntityOfPage"
                            | "http://www.w3.org/2000/01/rdf-schema#seeAlso"
                    ) {
                        out.document.link_candidates.push(target.clone());
                    }
                }
                out.statement(statement)?;
            }
        }
    }
    Ok(out.document)
}
fn resource(value: NamedOrBlankNode, scope: &str, blanks: &mut HashMap<String, String>) -> Term {
    match value {
        NamedOrBlankNode::NamedNode(node) => Term::iri(node.as_str()),
        NamedOrBlankNode::BlankNode(node) => blank(node.as_str(), scope, blanks),
    }
}
fn blank(value: &str, scope: &str, blanks: &mut HashMap<String, String>) -> Term {
    let next = blanks.len();
    Term::Blank {
        value: blanks
            .entry(value.into())
            .or_insert_with(|| format!("{scope}b{next}"))
            .clone(),
    }
}

/// Restrictive robots directives from HTML and HTTP headers.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct RobotsDirectives {
    /// Do not retain content or semantic assertions.
    pub noindex: bool,
    /// Do not schedule outgoing links.
    pub nofollow: bool,
}
impl RobotsDirectives {
    /// Merge comma/whitespace-separated tokens; restrictive values win.
    pub fn merge(&mut self, value: &str) {
        for token in value.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
            if token.eq_ignore_ascii_case("none") {
                self.noindex = true;
                self.nofollow = true;
            }
            if token.eq_ignore_ascii_case("noindex") {
                self.noindex = true;
            }
            if token.eq_ignore_ascii_case("nofollow") {
                self.nofollow = true;
            }
        }
    }
    /// Read generic and crawler-specific HTML meta directives.
    pub fn from_document(document: &WebDocument, agent: &str) -> Self {
        let mut directives = Self::default();
        for meta in &document.metadata {
            if meta.kind == "name"
                && (meta.name.eq_ignore_ascii_case("robots")
                    || meta.name.eq_ignore_ascii_case(agent))
            {
                directives.merge(&meta.content);
            }
        }
        directives
    }
}

struct LimitedJson {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for LimitedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("JSON-LD byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
