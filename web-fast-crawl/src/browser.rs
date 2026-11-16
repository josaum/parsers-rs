//! Optional Playwright renderer. All page HTTP traffic is fulfilled by the Rust
//! request bridge; no existing browser profile or authenticated session is used.
use crate::{
    http::{Fetched, Network},
    normalize_url,
    structured::{JsonSource, StructuredData, StructuredOptions},
};
use base64::Engine;
use futures_util::{stream::FuturesUnordered, FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

/// When to execute a page in the browser sidecar.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    /// HTTP/static extraction only; no Node or browser dependency at runtime.
    #[default]
    Disabled,
    /// Render sparse pages that contain scripts and no useful product JSON.
    Auto,
    /// Render each fetched HTML page, subject to robots and render limits.
    Always,
}
/// Isolated browser rendering options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserOptions {
    /// Rendering policy.
    pub mode: RenderMode,
    /// Node executable.
    pub node: String,
    /// Optional sidecar path; defaults to the script packaged with this crate.
    pub runner_path: Option<PathBuf>,
    /// Installed Playwright channel, normally `chrome`.
    pub channel: String,
    /// Run without a visible browser window. Native identity may include HeadlessChrome.
    pub headless: bool,
    /// Explicit Chromium executable instead of a channel.
    pub executable_path: Option<String>,
    /// Additional origins for scripts, styles, and APIs only; these do not expand crawl scope.
    pub resource_origins: Vec<String>,
    /// Exact POST URLs explicitly classified as read-only by the caller.
    pub read_only_post_urls: Vec<String>,
    /// Permit POSTs with a parsed GraphQL query operation. Mutations/subscriptions are refused.
    pub allow_graphql_queries: bool,
    /// Per-page rendering deadline, including browser startup.
    pub timeout_ms: u64,
    /// Maximum wait for network/DOM settling at each scroll stage.
    pub settle_ms: u64,
    /// DOM mutation quiet period.
    pub dom_quiet_ms: u64,
    /// Number of bounded scroll steps for lazy-loaded/infinite-scroll content.
    pub scroll_steps: usize,
    /// Maximum browser-generated HTTP requests per page, excluding the initial document.
    pub max_resources: usize,
    /// Maximum concurrent bridge fetches.
    pub resource_concurrency: usize,
    /// Skip image/media/font downloads while retaining their URLs in the DOM.
    pub block_images: bool,
}
impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            mode: RenderMode::Disabled,
            node: "node".into(),
            runner_path: None,
            channel: "chrome".into(),
            headless: true,
            executable_path: None,
            resource_origins: Vec::new(),
            read_only_post_urls: Vec::new(),
            allow_graphql_queries: true,
            timeout_ms: 20_000,
            settle_ms: 1_500,
            dom_quiet_ms: 200,
            scroll_steps: 2,
            max_resources: 64,
            resource_concurrency: 4,
            block_images: true,
        }
    }
}
impl BrowserOptions {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.timeout_ms == 0
            || self.timeout_ms > 300_000
            || self.settle_ms == 0
            || self.settle_ms > self.timeout_ms
            || self.dom_quiet_ms > self.settle_ms
            || self.scroll_steps > 32
            || self.max_resources == 0
            || self.max_resources > 1024
            || self.resource_concurrency == 0
            || self.resource_concurrency > 32
        {
            return Err("invalid browser limits".into());
        }
        for url in self
            .resource_origins
            .iter()
            .chain(&self.read_only_post_urls)
        {
            normalize_url(url, 8192)?;
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize)]
pub(crate) struct ResourceRequest {
    pub id: u64,
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub post_data: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub navigation: bool,
}
#[derive(Deserialize)]
struct State {
    key: String,
    value: Value,
}
#[derive(Deserialize)]
struct Snapshot {
    html_base64: String,
    final_url: String,
    states: Vec<State>,
    diagnostics: Vec<String>,
    shadow_roots: usize,
}
pub(crate) struct Rendered {
    pub html: Vec<u8>,
    pub url: String,
    pub data: StructuredData,
    pub diagnostics: Vec<String>,
    pub robots: crate::semantic::RobotsDirectives,
}

fn sidecar(
    options: &BrowserOptions,
) -> Result<
    (
        tokio::process::Child,
        tokio::process::ChildStdin,
        BufReader<tokio::process::ChildStdout>,
    ),
    String,
> {
    let runner = options
        .runner_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser/runner.mjs"));
    let mut child = Command::new(&options.node)
        .arg(runner)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "browser_sidecar_unavailable")?;
    let input = child.stdin.take().ok_or("browser_stdin_unavailable")?;
    let output = BufReader::new(child.stdout.take().ok_or("browser_stdout_unavailable")?);
    Ok((child, input, output))
}
async fn close_sidecar(mut child: tokio::process::Child, input: tokio::process::ChildStdin) {
    // Closing stdin also works when a timed-out write filled the pipe.
    drop(input);
    if tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}
pub(crate) async fn native_user_agent(
    options: &BrowserOptions,
    deadline: tokio::time::Instant,
) -> Result<String, String> {
    let (child, mut input, mut output) = sidecar(options)?;
    let work = async {
        write_message(
            &mut input,
            &json!({"type":"init", "identity_only":true, "options":options}),
        )
        .await?;
        let value = read_message(&mut output, &mut Vec::new(), 4096).await?;
        if value.get("type").and_then(Value::as_str) != Some("identity") {
            return Err("browser_identity_probe_failed".into());
        }
        value
            .get("user_agent")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "browser_identity_missing".into())
    };
    let result = tokio::time::timeout(
        Duration::from_millis(options.timeout_ms)
            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        work,
    )
    .await
    .unwrap_or_else(|_| Err("browser_identity_timeout".into()));
    close_sidecar(child, input).await;
    result
}

pub(crate) async fn render(
    network: &Network,
    initial: &Fetched,
    structured: &StructuredOptions,
    deadline: tokio::time::Instant,
) -> Result<Rendered, String> {
    let options = &network.options.browser;
    let (child, mut input, mut output) = sidecar(options)?;
    let init = json!({"type":"init", "url": initial.final_url.as_str(), "body":base64::engine::general_purpose::STANDARD.encode(&initial.body), "headers":headers(&initial.headers), "options":options, "max_html_bytes":network.options.max_response_bytes, "max_json_bytes":structured.max_json_bytes, "max_url_bytes":network.options.max_url_bytes, "max_redirects":network.options.max_redirects, "user_agent":network.options.user_agent});
    let work = async {
        write_message(&mut input, &init).await?;
        let mut captured = StructuredData::default();
        let mut robots = crate::semantic::RobotsDirectives::default();
        let mut pending = FuturesUnordered::new();
        let mut admitted = 0;
        let mut line = Vec::new();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(
            options
                .resource_concurrency
                .min(network.options.concurrency),
        ));
        let max_message = network
            .options
            .max_response_bytes
            .saturating_mul(2)
            .saturating_add(structured.max_json_bytes)
            .saturating_add(1024 * 1024);
        loop {
            tokio::select! {
                message = read_message(&mut output, &mut line, max_message) => {
                    let message = message?;
                    match message.get("type").and_then(Value::as_str) {
                        Some("request") => {
                            let request: ResourceRequest = serde_json::from_value(message).map_err(|_| "invalid_browser_request")?;
                            admitted += 1;
                            if admitted > options.max_resources || request.post_data.as_ref().is_some_and(|value| value.len() > 65536) || request.headers.iter().map(|(key,value)| key.len().saturating_add(value.len())).sum::<usize>() > 16384 {
                                write_message(&mut input, &json!({"type":"response", "id":request.id, "error":"browser_resource_limit"})).await?;
                            } else {
                                let permits = permits.clone();
                                pending.push(async move { let _permit = permits.acquire_owned().await.expect("local semaphore remains open"); let result = network.browser_resource(&request).await; (request, result) }.boxed());
                            }
                        }
                        Some("result") => {
                            let snapshot: Snapshot = serde_json::from_value(message).map_err(|_| "invalid_browser_result")?;
                            let url = normalize_url(&snapshot.final_url, network.options.max_url_bytes).map_err(|_| "invalid_browser_final_url")?;
                            if !network.permitted(&url) { return Err("browser_final_url_out_of_scope".into()); }
                            let html = base64::engine::general_purpose::STANDARD.decode(snapshot.html_base64).map_err(|_| "invalid_browser_html")?;
                            if html.len() > network.options.max_response_bytes { return Err("rendered_html_limit".into()); }
                            for state in snapshot.states { captured.add(JsonSource::new("browser_state", url.as_str(), &state.key, state.value), structured); }
                            let mut diagnostics = snapshot.diagnostics;
                            if !pending.is_empty() { diagnostics.push("resource_requests_incomplete".into()); }
                            if snapshot.shadow_roots > 0 { diagnostics.push(format!("open_shadow_roots:{}", snapshot.shadow_roots)); }
                            return Ok(Rendered { html, url: url.into(), data: captured, diagnostics, robots });
                        }
                        Some("error") => return Err(message.get("code").and_then(Value::as_str).unwrap_or("browser_render_failed").to_owned()),
                        _ => return Err("invalid_browser_protocol".into()),
                    }
                }
                completed = pending.next(), if !pending.is_empty() => {
                    let (request, response) = completed.ok_or("browser_fetch_failed")?;
                    let message = match response {
                        Ok(response) => {
                            let content_type = response.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
                            let mut directives = crate::semantic::RobotsDirectives::default();
                            crate::header_directives(&response, &network.options.agent, &mut directives);
                            if request.navigation {
                                robots.noindex |= directives.noindex;
                                robots.nofollow |= directives.nofollow;
                            }
                            if content_type.to_ascii_lowercase().contains("json") && !directives.noindex && !response.headers.contains_key("x-web-fast-synthetic") {
                                match serde_json::from_slice(&response.body) {
                                    Ok(value) => captured.add(JsonSource::new("network_json", response.final_url.as_str(), &request.method, value), structured),
                                    Err(_) => captured.diagnostics.push("invalid_network_json".into()),
                                }
                            }
                            json!({"type":"response", "id":request.id, "status":response.status, "headers":headers(&response.headers), "body":base64::engine::general_purpose::STANDARD.encode(response.body)})
                        }
                        Err(error) => {
                            if let Some(reason) = error.global_limit() { return Err(reason.into()); }
                            json!({"type":"response", "id":request.id, "error":"browser_request_denied_or_failed"})
                        }
                    };
                    write_message(&mut input, &message).await?;
                }
            }
        }
    };
    let result = tokio::time::timeout(
        Duration::from_millis(options.timeout_ms)
            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        work,
    )
    .await
    .unwrap_or_else(|_| Err("browser_timeout".into()));
    close_sidecar(child, input).await;
    result
}
fn headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "content-length"
                    | "content-encoding"
                    | "transfer-encoding"
                    | "connection"
                    | "set-cookie"
            )
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.into()))
        })
        .collect()
}
async fn write_message(
    writer: &mut (impl AsyncWriteExt + Unpin),
    value: &Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| "browser_protocol_encode")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| "browser_protocol_write".into())
}
async fn read_message(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    line: &mut Vec<u8>,
    limit: usize,
) -> Result<Value, String> {
    // The buffer survives cancellation by select! while another request finishes.
    let remaining = limit.saturating_add(1).saturating_sub(line.len());
    reader
        .take(remaining as u64)
        .read_until(b'\n', line)
        .await
        .map_err(|_| "browser_protocol_read")?;
    if line.len() > limit || line.last() != Some(&b'\n') {
        return Err("browser_protocol_limit_or_exit".into());
    }
    let value = serde_json::from_slice(line).map_err(|_| "browser_protocol_json".into());
    line.clear();
    value
}

pub(crate) enum GraphqlPolicy {
    Query,
    NeedsDocument,
    Blocked,
}
pub(crate) fn graphql_policy(body: &str) -> GraphqlPolicy {
    use graphql_parser::query::{Definition, OperationDefinition};
    if body.len() > 65536 {
        return GraphqlPolicy::Blocked;
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return GraphqlPolicy::Blocked;
    };
    let Some(query) = value.get("query").and_then(Value::as_str) else {
        return if value
            .pointer("/extensions/persistedQuery/sha256Hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            GraphqlPolicy::NeedsDocument
        } else {
            GraphqlPolicy::Blocked
        };
    };
    if !bounded_graphql(query) {
        return GraphqlPolicy::Blocked;
    }
    let Ok(document) = graphql_parser::parse_query::<String>(query) else {
        return GraphqlPolicy::Blocked;
    };
    let selected = value.get("operationName").and_then(Value::as_str);
    let mut operations = Vec::new();
    for definition in document.definitions {
        if let Definition::Operation(operation) = definition {
            let (name, is_query) = match operation {
                OperationDefinition::SelectionSet(_) => (None, true),
                OperationDefinition::Query(query) => (query.name, true),
                OperationDefinition::Mutation(mutation) => (mutation.name, false),
                OperationDefinition::Subscription(subscription) => (subscription.name, false),
            };
            if selected.is_none() || name.as_deref() == selected {
                operations.push(is_query);
            }
        }
    }
    if operations == [true] {
        GraphqlPolicy::Query
    } else {
        GraphqlPolicy::Blocked
    }
}
fn bounded_graphql(query: &str) -> bool {
    let bytes = query.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"\"\"\"") {
            i += 3;
            while i < bytes.len() && !bytes[i..].starts_with(b"\"\"\"") {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = i.saturating_add(3).min(bytes.len());
            continue;
        }
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = i.saturating_add(1).min(bytes.len());
            continue;
        }
        if matches!(bytes[i], b'{' | b'(' | b'[') {
            depth += 1;
            if depth > 64 {
                return false;
            }
        }
        if matches!(bytes[i], b'}' | b')' | b']') {
            depth = depth.saturating_sub(1);
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graphql_operations_are_parsed_instead_of_keyword_matched() {
        assert!(matches!(
            graphql_policy(r#"{"query":"query Catalog { products { name } }"}"#),
            GraphqlPolicy::Query
        ));
        assert!(matches!(
            graphql_policy(r#"{"query":"{ products(filter: \"mutation\") { name } }"}"#),
            GraphqlPolicy::Query
        ));
        assert!(matches!(
            graphql_policy(r#"{"query":"mutation Buy { purchase(id: 1) { id } }"}"#),
            GraphqlPolicy::Blocked
        ));
        assert!(matches!(
            graphql_policy(r#"{"query":"subscription Feed { products { name } }"}"#),
            GraphqlPolicy::Blocked
        ));
        assert!(matches!(
            graphql_policy(
                r#"{"operationName":"Safe","query":"query Safe { products { name } } mutation Bad { purchase { id } }"}"#
            ),
            GraphqlPolicy::Query
        ));
        assert!(matches!(
            graphql_policy(r#"{"query":"query A { a } query B { b }"}"#),
            GraphqlPolicy::Blocked
        ));
        let persisted =
            json!({"extensions":{"persistedQuery":{"sha256Hash":"a".repeat(64)}}}).to_string();
        assert!(matches!(
            graphql_policy(&persisted),
            GraphqlPolicy::NeedsDocument
        ));
        assert!(!bounded_graphql(&format!(
            "{}x{}",
            "{a ".repeat(100),
            "}".repeat(100)
        )));
    }
    #[tokio::test]
    async fn partial_protocol_lines_survive_select_cancellation() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();
        writer.write_all(b"{\"type\":").await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(5),
            read_message(&mut reader, &mut buffer, 1024)
        )
        .await
        .is_err());
        writer.write_all(b"\"result\"}\n").await.unwrap();
        assert_eq!(
            read_message(&mut reader, &mut buffer, 1024).await.unwrap()["type"],
            "result"
        );
        assert!(buffer.is_empty());
    }
}
