#![deny(missing_docs)]
//! Bounded HTTP crawling, offline semantic extraction, and indexed RDF graphs.
//! Static HTML parsing remains in the side-effect-free `web-fast-core` crate.
mod amazon_html;
pub mod browser;
mod commerce_html;
pub mod graph;
mod http;
pub mod semantic;
mod sitemap;
pub mod structured;
use futures_util::{stream, StreamExt};
use graph::{Graph, GraphBuilder, GraphError, GraphLimits, Term};
pub use http::normalize_url;
use http::{origin, FetchError, Fetched, Network};
use semantic::{extract_semantics, RobotsDirectives, SemanticOptions, DUPLICATE, REDIRECT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
use url::Url;
use web_fast_core::{parse_html_bytes, ParseOptions, WebDocument};

/// Crawl policies. Defaults restrict network requests to seed origins and public IPs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlOptions {
    /// Maximum scheduled page attempts, including failed pages.
    pub max_pages: usize,
    /// Maximum discovered unique page/sitemap URLs retained by the frontier.
    pub max_urls: usize,
    /// Maximum scheduled sitemaps, including nested indexes.
    pub max_sitemaps: usize,
    /// Maximum link distance from a seed. Sitemaps do not increase distance.
    pub max_depth: usize,
    /// Maximum simultaneous fetch jobs.
    pub concurrency: usize,
    /// Maximum simultaneous HTTP requests per origin.
    pub per_origin_concurrency: usize,
    /// Minimum interval between request starts on one origin.
    pub min_origin_interval_ms: u64,
    /// Total HTTP request attempts, including robots, redirects and retries.
    pub max_requests: usize,
    /// Decoded bytes across all successful response streams, including robots/sitemaps.
    pub max_total_bytes: usize,
    /// Maximum decoded bytes in one page or sitemap.
    pub max_response_bytes: usize,
    /// Maximum normalized URL length.
    pub max_url_bytes: usize,
    /// Total network/scheduler deadline; at most 24 hours to bound robots cache age.
    pub max_duration_ms: u64,
    /// Timeout per HTTP attempt including response-body reading.
    pub request_timeout_ms: u64,
    /// Maximum redirects for pages and robots, checked at every hop.
    pub max_redirects: usize,
    /// Additional attempts for transient failures.
    pub max_retries: usize,
    /// Do not retry a response asking to wait longer; still defer that origin.
    pub max_retry_delay_ms: u64,
    /// Product token for robots rules and agent-scoped indexing directives.
    pub agent: String,
    /// Explicit HTTP/browser User-Agent. With None, browser-enabled crawls use
    /// the selected browser's native identity; HTTP-only crawls omit this header.
    pub user_agent: Option<String>,
    /// Additional allowed HTTP(S) origins; seed origins are always included.
    pub allowed_origins: Vec<String>,
    /// Explicit opt-in for private addresses, intended for authorized internal crawling.
    pub allow_private_networks: bool,
    /// Follow sitemap URLs advertised in robots.txt.
    pub discover_sitemaps: bool,
    /// Follow in-scope schema:url/mainEntityOfPage/seeAlso relationships from JSON-LD.
    pub follow_semantic_links: bool,
    /// Case-insensitive terms used to prioritize anchor text and URLs at the same depth.
    pub focus_terms: Vec<String>,
    /// Keep full parsed documents in the report, except pages marked noindex.
    pub retain_documents: bool,
    /// HTML parser limits and decoding overrides.
    pub parser: ParseOptions,
    /// Semantic projection limits and trusted offline contexts.
    pub semantic: SemanticOptions,
    /// JSON capture and commerce normalization limits.
    pub structured: structured::StructuredOptions,
    /// Optional isolated JavaScript rendering.
    pub browser: browser::BrowserOptions,
    /// Graph limits.
    pub graph: GraphLimits,
}
impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            max_pages: 100,
            max_urls: 10_000,
            max_sitemaps: 32,
            max_depth: 3,
            concurrency: 4,
            per_origin_concurrency: 2,
            min_origin_interval_ms: 200,
            max_requests: 1_000,
            max_total_bytes: 64 * 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_url_bytes: 8192,
            max_duration_ms: 300_000,
            request_timeout_ms: 30_000,
            max_redirects: 8,
            max_retries: 2,
            max_retry_delay_ms: 30_000,
            agent: "web-fast-crawl".into(),
            user_agent: None,
            allowed_origins: Vec::new(),
            allow_private_networks: false,
            discover_sitemaps: true,
            follow_semantic_links: true,
            focus_terms: Vec::new(),
            retain_documents: false,
            parser: ParseOptions::default(),
            semantic: SemanticOptions::default(),
            structured: structured::StructuredOptions::default(),
            browser: browser::BrowserOptions::default(),
            graph: GraphLimits::default(),
        }
    }
}
/// Configuration/setup failures. Individual URL failures are recorded in the report.
#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    /// Invalid options or seeds.
    #[error("invalid crawl configuration: {0}")]
    Configuration(String),
    /// HTTP client initialization failed.
    #[error("HTTP client initialization failed: {0}")]
    Client(#[from] reqwest::Error),
}
/// Outcome of one scheduled page or sitemap.
#[derive(Debug, Serialize)]
pub struct PageRecord {
    /// Normalized requested URL.
    pub url: String,
    /// Final URL after permitted redirects, when available.
    pub final_url: Option<String>,
    /// Link depth.
    pub depth: usize,
    /// `page` or `sitemap`.
    pub kind: String,
    /// Final HTTP status when available.
    pub status: Option<u16>,
    /// SHA-256 of the response bytes, when parsed.
    pub source_sha256: Option<String>,
    /// Earlier page with byte-identical content; identities are not merged.
    pub duplicate_of: Option<String>,
    /// Restrictive directives applied to this page.
    pub robots: RobotsDirectives,
    /// Title, omitted for noindex pages.
    pub title: Option<String>,
    /// Parsed content if explicitly requested and indexing is permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<WebDocument>,
    /// Captured API/application JSON and commerce records; absent for noindex pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<structured::StructuredData>,
    /// Final browser URL when JavaScript rendering was used.
    pub rendered_url: Option<String>,
    /// SHA-256 of the rendered DOM snapshot, distinct from the HTTP body digest.
    pub rendered_sha256: Option<String>,
    /// Payload-free error/diagnostic codes.
    pub diagnostics: Vec<String>,
}
/// A bounded crawl result; stopping on a limit preserves all completed evidence.
#[derive(Debug, Serialize)]
pub struct CrawlReport {
    /// User-Agent used for HTTP and browser navigation, or None if omitted.
    pub user_agent: Option<String>,
    /// `complete`, `page_limit`, `url_limit`, `sitemap_limit`, `request_limit`,
    /// `byte_limit`, `deadline`, `graph_limit`, or `graph_error`.
    pub stop_reason: String,
    /// HTTP attempts actually started.
    pub requests: usize,
    /// Decoded response bytes charged to the crawl budget.
    pub response_bytes: usize,
    /// Number of unique URLs admitted to the frontier.
    pub discovered_urls: usize,
    /// Completed page/sitemap outcomes in deterministic scheduling order.
    pub pages: Vec<PageRecord>,
    /// Interned semantic and crawl graph, with indexes for in-process queries.
    pub graph: Graph,
}
/// Reusable crawler configuration; each crawl owns an independent connection pool/frontier.
pub struct Crawler {
    options: Arc<CrawlOptions>,
}
impl Crawler {
    /// Validate options without performing any network requests.
    pub fn new(mut options: CrawlOptions) -> Result<Self, CrawlError> {
        if options.max_pages == 0
            || options.max_urls == 0
            || options.max_requests == 0
            || options.max_response_bytes == 0
            || options.max_total_bytes == 0
            || options.max_url_bytes == 0
            || options.concurrency == 0
            || options.concurrency > 256
            || options.per_origin_concurrency == 0
            || options.per_origin_concurrency > options.concurrency
            || options.request_timeout_ms == 0
            || options.request_timeout_ms > 86_400_000
            || options.max_duration_ms == 0
            || options.max_duration_ms > 86_400_000
            || options.max_retries > 8
            || options.max_redirects > 32
            || options.min_origin_interval_ms > 86_400_000
            || options.max_retry_delay_ms > 86_400_000
        {
            return Err(CrawlError::Configuration(
                "invalid budget, concurrency, or timeout".into(),
            ));
        }
        if options.agent.is_empty()
            || options.agent.len() > 64
            || !options
                .agent
                .bytes()
                .all(|b| b.is_ascii_alphabetic() || matches!(b, b'-' | b'_'))
        {
            return Err(CrawlError::Configuration(
                "agent must be an ASCII RFC 9309 product token".into(),
            ));
        }
        options
            .browser
            .validate()
            .map_err(CrawlError::Configuration)?;
        if let Some(value) = &options.user_agent {
            validate_user_agent(value).map_err(CrawlError::Configuration)?;
        }
        let mut seen = HashSet::new();
        options.focus_terms.retain_mut(|term| {
            *term = term.trim().to_lowercase();
            !term.is_empty() && seen.insert(term.clone())
        });
        Ok(Self {
            options: Arc::new(options),
        })
    }
    /// Crawl seed URLs, respecting robots, origin scope, and all configured limits.
    /// Browser execution is optional; no existing browser session is imported.
    pub async fn crawl(&self, seeds: &[String]) -> Result<CrawlReport, CrawlError> {
        let mut options = self.options.clone();
        let deadline = Instant::now() + Duration::from_millis(options.max_duration_ms);
        if seeds.is_empty() || seeds.len() > options.max_urls {
            return Err(CrawlError::Configuration(
                "supply between one and max_urls seeds".into(),
            ));
        }
        let seeds = seeds
            .iter()
            .map(|url| normalize_url(url, options.max_url_bytes).map_err(CrawlError::Configuration))
            .collect::<Result<Vec<_>, _>>()?;
        let mut allowed: HashSet<_> = seeds.iter().map(origin).collect();
        for value in &options.allowed_origins {
            allowed.insert(origin(
                &normalize_url(value, options.max_url_bytes).map_err(CrawlError::Configuration)?,
            ));
        }
        if options.user_agent.is_none() && options.browser.mode != browser::RenderMode::Disabled {
            let identity = browser::native_user_agent(&options.browser, deadline)
                .await
                .map_err(|code| {
                    CrawlError::Configuration(format!("browser identity unavailable: {code}"))
                })?;
            validate_user_agent(&identity).map_err(CrawlError::Configuration)?;
            Arc::make_mut(&mut options).user_agent = Some(identity);
        }
        let network = Network::new(options.clone(), allowed)?;
        if seeds.iter().any(|url| !network.permitted(url)) {
            return Err(CrawlError::Configuration(
                "seed destination denied; internal crawling requires explicit opt-in".into(),
            ));
        }
        let mut frontier = Frontier::new(options.max_urls);
        for url in seeds {
            frontier.push(url, 0, Kind::Page, 0);
        }
        let mut graph = GraphBuilder::new(options.graph.clone());
        let mut pages = Vec::new();
        let mut hashes: HashMap<String, String> = HashMap::new();
        let mut completed_urls = HashSet::new();
        let mut scheduled_pages = 0;
        let mut scheduled_sitemaps = 0;
        let mut sitemap_limited = false;
        let mut stop = "complete";
        'crawl: while !frontier.queue.is_empty() {
            if Instant::now() >= deadline {
                stop = "deadline";
                break;
            }
            let mut jobs = Vec::new();
            while jobs.len() < options.concurrency {
                let Some(job) = frontier.queue.pop() else {
                    break;
                };
                if completed_urls.contains(&(job.url.clone(), job.kind)) {
                    continue;
                }
                if job.kind == Kind::Page {
                    if scheduled_pages == options.max_pages {
                        frontier.queue.push(job);
                        break;
                    }
                    scheduled_pages += 1;
                } else {
                    if scheduled_sitemaps == options.max_sitemaps {
                        sitemap_limited = true;
                        continue;
                    }
                    scheduled_sitemaps += 1;
                }
                jobs.push(job);
            }
            if jobs.is_empty() {
                if !frontier.queue.is_empty() {
                    stop = "page_limit";
                }
                break;
            }
            let fetches = stream::iter(jobs.into_iter().map(|job| async {
                let result = network
                    .fetch(
                        &job.url,
                        options
                            .max_response_bytes
                            .min(options.parser.max_input_bytes),
                    )
                    .await;
                (job, result)
            }))
            .buffered(options.concurrency)
            .collect::<Vec<_>>();
            let fetched = match tokio::time::timeout_at(deadline, fetches).await {
                Ok(results) => results,
                Err(_) => {
                    stop = "deadline";
                    break;
                }
            };
            for (job, result) in fetched {
                let mut record = PageRecord {
                    url: job.url.to_string(),
                    final_url: None,
                    depth: job.depth,
                    kind: job.kind.name().into(),
                    status: None,
                    source_sha256: None,
                    duplicate_of: None,
                    robots: RobotsDirectives::default(),
                    title: None,
                    document: None,
                    structured: None,
                    rendered_url: None,
                    rendered_sha256: None,
                    diagnostics: Vec::new(),
                };
                let response = match result {
                    Ok(response) => response,
                    Err(error) => {
                        if let FetchError::Status(status) | FetchError::Challenge(status) = error {
                            record.status = Some(status);
                        }
                        record.diagnostics.push(error.to_string());
                        let limit = error.global_limit();
                        pages.push(record);
                        if let Some(reason) = limit {
                            stop = reason;
                            break 'crawl;
                        }
                        continue;
                    }
                };
                record.final_url = Some(response.final_url.to_string());
                record.status = Some(response.status);
                for (from, to) in &response.redirects {
                    if let Err(error) = graph.insert(
                        Term::iri(from.as_str()),
                        Term::iri(REDIRECT),
                        Term::iri(to.as_str()),
                        None,
                        Term::iri(job.url.as_str()),
                    ) {
                        stop = record_graph_error(error, &mut record);
                        pages.push(record);
                        break 'crawl;
                    }
                }
                let already_completed =
                    !completed_urls.insert((response.final_url.clone(), job.kind));
                completed_urls.insert((job.url.clone(), job.kind));
                if already_completed {
                    record
                        .diagnostics
                        .push("redirect_target_already_processed".into());
                    pages.push(record);
                    continue;
                }
                if options.discover_sitemaps {
                    // Cached by fetch(), so this does not perform an additional request.
                    if let Ok(policy) = network.robots(&response.final_url).await {
                        for url in &policy.sitemaps {
                            frontier.push(url.clone(), job.depth, Kind::Sitemap, 0);
                        }
                    }
                }
                if job.kind == Kind::Sitemap {
                    match sitemap::parse(&response.body, options.max_urls, options.max_url_bytes) {
                        Ok((kind, urls)) => {
                            for url in urls {
                                if network.permitted(&url) {
                                    frontier.push(url, job.depth, kind, 0);
                                }
                            }
                        }
                        Err(error) => record.diagnostics.push(error.into()),
                    }
                    pages.push(record);
                    continue;
                }
                let (mut document, direct_json) = match parse_response(&response, &options.parser) {
                    Ok(document) => document,
                    Err(error) => {
                        record.diagnostics.push(error);
                        pages.push(record);
                        continue;
                    }
                };
                let source_hash = document.source_sha256.clone();
                record.source_sha256 = Some(source_hash.clone());
                record.robots = RobotsDirectives::from_document(&document, &options.agent);
                header_directives(&response, &options.agent, &mut record.robots);
                let is_json = direct_json.is_some();
                let mut data = structured::StructuredData::default();
                if !record.robots.noindex {
                    if let Some(value) = direct_json {
                        data.add(
                            structured::JsonSource::new(
                                "http_json",
                                response.final_url.as_str(),
                                "GET",
                                value,
                            ),
                            &options.structured,
                        );
                    } else {
                        data = structured::StructuredData::from_document(
                            &document,
                            response.final_url.as_str(),
                            &options.structured,
                        );
                        commerce_html::capture(
                            &mut data,
                            &response.body,
                            response.final_url.as_str(),
                            &options.structured,
                        );
                    }
                    data.normalize(&options.structured);
                    let should_render = !is_json
                        && match options.browser.mode {
                            browser::RenderMode::Disabled => false,
                            browser::RenderMode::Always => true,
                            browser::RenderMode::Auto => {
                                data.products.is_empty()
                                    && document.text.len() < 200
                                    && response
                                        .body
                                        .windows(7)
                                        .any(|part| part.eq_ignore_ascii_case(b"<script"))
                            }
                        };
                    if should_render {
                        match browser::render(&network, &response, &options.structured, deadline)
                            .await
                        {
                            Ok(rendered) => {
                                record.diagnostics.extend(rendered.diagnostics);
                                record.robots.noindex |= rendered.robots.noindex;
                                record.robots.nofollow |= rendered.robots.nofollow;
                                let mut parse_options = options.parser.clone();
                                parse_options.encoding_label = Some("utf-8".into());
                                match parse_html_bytes(
                                    &rendered.html,
                                    Some(&rendered.url),
                                    parse_options,
                                ) {
                                    Ok(parsed) => {
                                        record.rendered_sha256 = Some(parsed.source_sha256.clone());
                                        record.rendered_url = Some(rendered.url.clone());
                                        let directives = RobotsDirectives::from_document(
                                            &parsed,
                                            &options.agent,
                                        );
                                        record.robots.noindex |= directives.noindex;
                                        record.robots.nofollow |= directives.nofollow;
                                        if !record.robots.noindex {
                                            let mut extracted =
                                                structured::StructuredData::from_document(
                                                    &parsed,
                                                    &rendered.url,
                                                    &options.structured,
                                                );
                                            commerce_html::capture(
                                                &mut extracted,
                                                &rendered.html,
                                                &rendered.url,
                                                &options.structured,
                                            );
                                            for source in extracted
                                                .sources
                                                .into_iter()
                                                .chain(rendered.data.sources)
                                            {
                                                data.add(source, &options.structured);
                                            }
                                            data.diagnostics.extend(extracted.diagnostics);
                                            data.diagnostics.extend(rendered.data.diagnostics);
                                        }
                                        document = parsed;
                                    }
                                    Err(error) => record
                                        .diagnostics
                                        .push(format!("rendered_parse_failed:{error}")),
                                }
                                data.normalize(&options.structured);
                            }
                            Err(error) => {
                                record.diagnostics.push(error.clone());
                                if matches!(error.as_str(), "request_limit" | "byte_limit") {
                                    stop = if error == "request_limit" {
                                        "request_limit"
                                    } else {
                                        "byte_limit"
                                    };
                                    pages.push(record);
                                    break 'crawl;
                                }
                            }
                        }
                    }
                }
                let effective_source = record
                    .rendered_url
                    .as_deref()
                    .unwrap_or(response.final_url.as_str())
                    .to_owned();
                record
                    .diagnostics
                    .extend(document.diagnostics.iter().cloned());
                if !record.robots.noindex {
                    record.title = document.title.clone();
                    if let Some(first) = hashes.get(&source_hash) {
                        record.duplicate_of = Some(first.clone());
                        if let Err(error) = graph.insert(
                            Term::iri(response.final_url.as_str()),
                            Term::iri(DUPLICATE),
                            Term::iri(first),
                            None,
                            Term::iri(response.final_url.as_str()),
                        ) {
                            stop = record_graph_error(error, &mut record);
                            pages.push(record);
                            break 'crawl;
                        }
                    } else {
                        hashes.insert(source_hash.clone(), response.final_url.to_string());
                    }
                }
                if job.depth < options.max_depth && !record.robots.nofollow {
                    for link in document.links.iter().chain(&document.discovery_links) {
                        if link.rel.as_deref().is_some_and(|rel| {
                            rel.split_ascii_whitespace()
                                .any(|part| part.eq_ignore_ascii_case("nofollow"))
                        }) {
                            continue;
                        }
                        if let Some(url) = link
                            .resolved_url
                            .as_deref()
                            .and_then(|url| normalize_url(url, options.max_url_bytes).ok())
                            .filter(|url| network.permitted(url))
                        {
                            let score = focus_score(&url, &link.text, &options.focus_terms);
                            frontier.push(url, job.depth + 1, Kind::Page, score);
                        }
                    }
                }
                if !record.robots.noindex {
                    if job.depth < options.max_depth && !record.robots.nofollow {
                        for hint in &data.pagination {
                            if hint.has_next_page == Some(false) {
                                continue;
                            }
                            if let Some(url) = hint
                                .next_url
                                .as_deref()
                                .and_then(|value| normalize_url(value, options.max_url_bytes).ok())
                                .filter(|url| network.permitted(url))
                            {
                                frontier.push(url, job.depth + 1, Kind::Page, 1);
                            }
                        }
                    }
                    match extract_semantics(&document, &effective_source, &options.semantic) {
                        Ok(mut semantic) => {
                            if is_json {
                                for statement in &mut semantic.statements {
                                    if statement.subject == Term::iri(&effective_source)
                                        && statement.predicate == Term::iri(semantic::RDF_TYPE)
                                        && statement.object
                                            == Term::iri("https://schema.org/WebPage")
                                    {
                                        statement.object =
                                            Term::iri("https://schema.org/DigitalDocument");
                                    }
                                }
                            }
                            record
                                .diagnostics
                                .extend(semantic.diagnostics.iter().cloned());
                            if options.follow_semantic_links
                                && job.depth < options.max_depth
                                && !record.robots.nofollow
                            {
                                for target in &semantic.link_candidates {
                                    if let Ok(url) = normalize_url(target, options.max_url_bytes) {
                                        if network.permitted(&url) {
                                            let score = focus_score(&url, "", &options.focus_terms);
                                            frontier.push(url, job.depth + 1, Kind::Page, score);
                                        }
                                    }
                                }
                            }
                            if let Err(error) = semantic.insert_into(&mut graph) {
                                stop = record_graph_error(error, &mut record);
                                pages.push(record);
                                break 'crawl;
                            }
                        }
                        Err(error) => record
                            .diagnostics
                            .push(format!("semantic_projection_failed:{error}")),
                    }
                    if let Err(error) = data.insert_products(&mut graph, &effective_source) {
                        stop = record_graph_error(error, &mut record);
                        pages.push(record);
                        break 'crawl;
                    }
                    record.diagnostics.extend(data.diagnostics.iter().cloned());
                    record.structured = Some(data);
                    if options.retain_documents && !is_json {
                        record.document = Some(document);
                    }
                }
                pages.push(record);
                if Instant::now() >= deadline {
                    stop = "deadline";
                    break 'crawl;
                }
            }
        }
        if stop == "complete" && frontier.limited {
            stop = "url_limit";
        }
        if stop == "complete" && sitemap_limited {
            stop = "sitemap_limit";
        }
        let (requests, response_bytes) = network.counts();
        Ok(CrawlReport {
            user_agent: options.user_agent.clone(),
            stop_reason: stop.into(),
            requests,
            response_bytes,
            discovered_urls: frontier.seen.len(),
            pages,
            graph: graph.finish(),
        })
    }
}
fn validate_user_agent(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 2048 || !value.bytes().all(|b| (32..=126).contains(&b)) {
        return Err("User-Agent must contain 1..=2048 printable ASCII bytes".into());
    }
    Ok(())
}
fn record_graph_error(error: GraphError, record: &mut PageRecord) -> &'static str {
    record.diagnostics.push(error.to_string());
    match error {
        GraphError::Limit(_) => "graph_limit",
        GraphError::Invalid(_) => "graph_error",
    }
}
fn parse_response(
    response: &Fetched,
    options: &ParseOptions,
) -> Result<(WebDocument, Option<serde_json::Value>), String> {
    let mime: mime::Mime = response
        .headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or("missing_content_type")?
        .parse()
        .map_err(|_| "invalid_content_type")?;
    if mime.essence_str() == "application/json"
        || mime.suffix().is_some_and(|suffix| suffix == mime::JSON)
    {
        let value: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| "invalid_http_json")?;
        let mut document =
            parse_html_bytes(b"", Some(response.final_url.as_str()), options.clone())
                .map_err(|error| error.to_string())?;
        document.source_sha256 = format!("{:x}", Sha256::digest(&response.body));
        if mime.essence_str() == "application/ld+json" || value.get("@context").is_some() {
            document.json_ld.push(value.clone());
        }
        return Ok((document, Some(value)));
    }
    if !matches!(mime.essence_str(), "text/html" | "application/xhtml+xml") {
        return Err("unsupported_content_type".into());
    }
    let mut options = options.clone();
    let has_bom = response.body.starts_with(&[0xef, 0xbb, 0xbf])
        || response.body.starts_with(&[0xff, 0xfe])
        || response.body.starts_with(&[0xfe, 0xff]);
    if options.encoding_label.is_none() && !has_bom {
        options.encoding_label = mime
            .get_param("charset")
            .map(|value| value.as_str().to_owned());
    }
    parse_html_bytes(&response.body, Some(response.final_url.as_str()), options)
        .map(|document| (document, None))
        .map_err(|error| format!("html_parse_failed:{error}"))
}
fn header_directives(response: &Fetched, agent: &str, directives: &mut RobotsDirectives) {
    for value in response.headers.get_all("x-robots-tag") {
        let Ok(value) = value.to_str() else {
            continue;
        };
        let mut applies = true;
        for part in value.split(',') {
            if let Some((name, value)) = part.split_once(':') {
                applies = name.trim().eq_ignore_ascii_case(agent) || name.trim() == "*";
                if applies {
                    directives.merge(value);
                }
            } else if applies {
                directives.merge(part);
            }
        }
    }
}
fn focus_score(url: &Url, text: &str, terms: &[String]) -> usize {
    if terms.is_empty() {
        return 0;
    }
    let target = format!("{} {}", url.as_str(), text).to_lowercase();
    terms
        .iter()
        .filter(|term| !term.is_empty() && target.contains(term.as_str()))
        .count()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Page,
    Sitemap,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Sitemap => "sitemap",
        }
    }
}
#[derive(Debug, Eq)]
struct Job {
    url: Url,
    depth: usize,
    kind: Kind,
    score: usize,
    sequence: usize,
}
impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence
    }
}
impl Ord for Job {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .depth
            .cmp(&self.depth)
            .then(self.score.cmp(&other.score))
            .then(other.sequence.cmp(&self.sequence))
    }
}
impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
struct Frontier {
    queue: BinaryHeap<Job>,
    seen: HashSet<(Url, Kind)>,
    max: usize,
    limited: bool,
}
impl Frontier {
    fn new(max: usize) -> Self {
        Self {
            queue: BinaryHeap::new(),
            seen: HashSet::new(),
            max,
            limited: false,
        }
    }
    fn push(&mut self, url: Url, depth: usize, kind: Kind, score: usize) {
        if self.seen.contains(&(url.clone(), kind)) {
            return;
        }
        if self.seen.len() >= self.max {
            self.limited = true;
            return;
        }
        let sequence = self.seen.len();
        self.seen.insert((url.clone(), kind));
        self.queue.push(Job {
            url,
            depth,
            kind,
            score,
            sequence,
        });
    }
}
