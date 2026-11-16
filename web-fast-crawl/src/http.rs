//! HTTP policy, connection pooling, bounded reads, and origin scheduling.
use crate::CrawlOptions;
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, CONTENT_LENGTH, LOCATION, RETRY_AFTER},
    Client, StatusCode,
};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Mutex as AsyncMutex, OnceCell, Semaphore},
    time::Instant,
};
use url::{Host, Url};

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum FetchError {
    #[error("URL rejected by crawl scope or network policy")]
    Policy,
    #[error("HTTP transport failed or timed out")]
    Transport,
    #[error("request budget exhausted")]
    Requests,
    #[error("total response byte budget exhausted")]
    Bytes,
    #[error("response exceeds byte limit")]
    Body,
    #[error("redirect loop, invalid target, or redirect limit")]
    Redirect,
    #[error("robots policy disallows this URL")]
    Robots,
    #[error("redirect destination is disallowed by robots policy")]
    RobotsRedirect,
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("website access challenge (HTTP {0})")]
    Challenge(u16),
}
impl FetchError {
    pub fn global_limit(&self) -> Option<&'static str> {
        match self {
            Self::Requests => Some("request_limit"),
            Self::Bytes => Some("byte_limit"),
            _ => None,
        }
    }
}

/// Canonical crawl identity: HTTP(S), no credentials, no fragment, and normalized
/// unreserved percent escapes. Query order and values are preserved.
pub fn normalize_url(value: &str, max_bytes: usize) -> Result<Url, String> {
    if value.len() > max_bytes {
        return Err("URL byte limit".into());
    }
    let mut url = Url::parse(value).map_err(|_| "invalid URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("only HTTP(S) URLs without credentials are accepted".into());
    }
    url.set_fragment(None);
    let path = normalize_escapes(url.path());
    url.set_path(&path);
    if let Some(query) = url.query() {
        let query = normalize_escapes(query);
        url.set_query(Some(&query));
    }
    if url.as_str().len() > max_bytes {
        return Err("URL byte limit".into());
    }
    // WHATWG URL parsing accepts some strings (for example malformed percent
    // escapes) that cannot be RDF resource identifiers in the crawl graph.
    oxrdf::NamedNodeRef::new(url.as_str()).map_err(|_| "URL is not a valid RDF IRI")?;
    Ok(url)
}
fn normalize_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    out.push(byte);
                } else {
                    out.extend_from_slice(format!("%{byte:02X}").as_bytes());
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // URL path and query serializations are ASCII or valid UTF-8.
    String::from_utf8(out).expect("normalizing ASCII escapes preserves UTF-8")
}
pub(crate) fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_documentation()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && matches!(b, 18 | 19))
                || (a == 192 && b == 0 && c == 0))
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(ip));
            }
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] == 0xdb8 || s[1] < 0x200))
        }
    }
}
struct GuardedDns {
    allow_private: bool,
}
impl Resolve for GuardedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            if addresses.is_empty()
                || (!allow_private && addresses.iter().any(|address| !public_ip(address.ip())))
            {
                return Err("DNS destination denied".into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}
struct OriginState {
    permits: Arc<Semaphore>,
    next_start: AsyncMutex<Instant>,
    robots: OnceCell<Arc<RobotsPolicy>>,
}
impl OriginState {
    async fn wait_start(&self, interval: Duration) {
        let mut next = self.next_start.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + interval;
    }
}
#[derive(Debug)]
pub(crate) struct RobotsPolicy {
    body: String,
    deny_all: bool,
    pub sitemaps: Vec<Url>,
}
impl RobotsPolicy {
    fn denied() -> Self {
        Self {
            body: String::new(),
            deny_all: true,
            sitemaps: Vec::new(),
        }
    }
    fn allows(&self, agent: &str, url: &Url) -> bool {
        !self.deny_all
            && robotstxt::DefaultMatcher::default().one_agent_allowed_by_robots(
                &self.body,
                agent,
                url.as_str(),
            )
    }
}
pub(crate) struct Fetched {
    pub final_url: Url,
    pub redirects: Vec<(Url, Url)>,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub status: u16,
}
struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}
pub(crate) struct Network {
    client: Client,
    pub options: Arc<CrawlOptions>,
    allowed: HashSet<String>,
    resource_allowed: HashSet<String>,
    origins: Mutex<HashMap<String, Arc<OriginState>>>,
    requests: AtomicUsize,
    bytes: AtomicUsize,
}
impl Network {
    pub fn new(
        options: Arc<CrawlOptions>,
        allowed: HashSet<String>,
    ) -> Result<Self, reqwest::Error> {
        let mut resource_allowed = allowed.clone();
        for value in &options.browser.resource_origins {
            if let Ok(url) = Url::parse(value) {
                resource_allowed.insert(origin(&url));
            }
        }
        let mut client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_millis(options.request_timeout_ms))
            .connect_timeout(Duration::from_millis(
                options.request_timeout_ms.min(10_000),
            ))
            .dns_resolver(Arc::new(GuardedDns {
                allow_private: options.allow_private_networks,
            }))
            .pool_max_idle_per_host(options.per_origin_concurrency);
        if let Some(user_agent) = &options.user_agent {
            client = client.user_agent(user_agent);
        }
        let client = client.build()?;
        Ok(Self {
            client,
            options,
            allowed,
            resource_allowed,
            origins: Mutex::new(HashMap::new()),
            requests: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        })
    }
    pub fn counts(&self) -> (usize, usize) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
    pub fn permitted(&self, url: &Url) -> bool {
        if !self.allowed.contains(&origin(url)) {
            return false;
        }
        self.address_permitted(url)
    }
    fn permitted_resource(&self, url: &Url) -> bool {
        self.resource_allowed.contains(&origin(url)) && self.address_permitted(url)
    }
    fn address_permitted(&self, url: &Url) -> bool {
        self.options.allow_private_networks
            || match url.host() {
                Some(Host::Ipv4(ip)) => public_ip(IpAddr::V4(ip)),
                Some(Host::Ipv6(ip)) => public_ip(IpAddr::V6(ip)),
                Some(Host::Domain(host)) => {
                    !host.eq_ignore_ascii_case("localhost")
                        && !host.to_ascii_lowercase().ends_with(".localhost")
                }
                None => false,
            }
    }
    fn state(&self, url: &Url) -> Arc<OriginState> {
        self.origins
            .lock()
            .unwrap()
            .entry(origin(url))
            .or_insert_with(|| {
                Arc::new(OriginState {
                    permits: Arc::new(Semaphore::new(self.options.per_origin_concurrency)),
                    next_start: AsyncMutex::new(Instant::now()),
                    robots: OnceCell::new(),
                })
            })
            .clone()
    }
    fn redirect(&self, url: &Url, headers: &HeaderMap) -> Result<Url, FetchError> {
        let value = headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(FetchError::Redirect)?;
        let url = url.join(value).map_err(|_| FetchError::Redirect)?;
        let url = normalize_url(url.as_str(), self.options.max_url_bytes)
            .map_err(|_| FetchError::Redirect)?;
        if !self.permitted_resource(&url) {
            return Err(FetchError::Policy);
        }
        Ok(url)
    }
    pub async fn robots(&self, url: &Url) -> Result<Arc<RobotsPolicy>, FetchError> {
        if !self.permitted_resource(url) {
            return Err(FetchError::Policy);
        }
        let state = self.state(url);
        let policy = state
            .robots
            .get_or_try_init(|| async {
                let mut target = url.clone();
                target.set_path("/robots.txt");
                target.set_query(None);
                target.set_fragment(None);
                let mut seen = HashSet::new();
                for _ in 0..=self.options.max_redirects {
                    if !seen.insert(target.clone()) {
                        return Ok(Arc::new(RobotsPolicy::denied()));
                    }
                    let response = match self.request(&target, 512 * 1024).await {
                        Ok(response) => response,
                        Err(error @ (FetchError::Requests | FetchError::Bytes)) => {
                            return Err(error)
                        }
                        Err(_) => return Ok(Arc::new(RobotsPolicy::denied())),
                    };
                    if is_redirect(response.status) {
                        target = match self.redirect(&target, &response.headers) {
                            Ok(url) => url,
                            Err(_) => return Ok(Arc::new(RobotsPolicy::denied())),
                        };
                        continue;
                    }
                    if response.status.is_success() {
                        let body = String::from_utf8_lossy(&response.body).into_owned();
                        let sitemaps = body
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("sitemap"))
                            .filter_map(|(_, value)| {
                                normalize_url(value.trim(), self.options.max_url_bytes).ok()
                            })
                            .filter(|url| self.permitted(url))
                            .take(self.options.max_sitemaps)
                            .collect();
                        return Ok(Arc::new(RobotsPolicy {
                            body,
                            deny_all: false,
                            sitemaps,
                        }));
                    }
                    let deny_all = !response.status.is_client_error()
                        || matches!(response.status.as_u16(), 401 | 403 | 429);
                    return Ok(Arc::new(RobotsPolicy {
                        body: String::new(),
                        deny_all,
                        sitemaps: Vec::new(),
                    }));
                }
                Ok(Arc::new(RobotsPolicy::denied()))
            })
            .await?;
        Ok(policy.clone())
    }
    pub async fn fetch(&self, start: &Url, body_limit: usize) -> Result<Fetched, FetchError> {
        let mut url = start.clone();
        let mut redirects = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..=self.options.max_redirects {
            if !self.permitted(&url) {
                return Err(FetchError::Policy);
            }
            if !seen.insert(url.clone()) {
                return Err(FetchError::Redirect);
            }
            if !self.robots(&url).await?.allows(&self.options.agent, &url) {
                return Err(if redirects.is_empty() {
                    FetchError::Robots
                } else {
                    FetchError::RobotsRedirect
                });
            }
            let response = self.request(&url, body_limit).await?;
            if is_redirect(response.status) {
                let next = self.redirect(&url, &response.headers)?;
                redirects.push((url, next.clone()));
                url = next;
                continue;
            }
            if !response.status.is_success() {
                return Err(FetchError::Status(response.status.as_u16()));
            }
            return Ok(Fetched {
                final_url: url,
                redirects,
                headers: response.headers,
                body: response.body,
                status: response.status.as_u16(),
            });
        }
        Err(FetchError::Redirect)
    }
    async fn request(&self, url: &Url, body_limit: usize) -> Result<Raw, FetchError> {
        self.request_with(
            url,
            body_limit,
            reqwest::Method::GET,
            HeaderMap::new(),
            None,
        )
        .await
    }
    async fn request_with(
        &self,
        url: &Url,
        body_limit: usize,
        method: reqwest::Method,
        headers: HeaderMap,
        body: Option<String>,
    ) -> Result<Raw, FetchError> {
        if !self.permitted_resource(url) {
            return Err(FetchError::Policy);
        }
        let state = self.state(url);
        for attempt in 0..=self.options.max_retries {
            let permit = state
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| FetchError::Transport)?;
            state
                .wait_start(Duration::from_millis(self.options.min_origin_interval_ms))
                .await;
            self.requests
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value
                        .checked_add(1)
                        .filter(|n| *n <= self.options.max_requests)
                })
                .map_err(|_| FetchError::Requests)?;
            let mut request = self
                .client
                .request(method.clone(), url.clone())
                .headers(headers.clone());
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let response = request.send().await;
            let mut response = match response {
                Ok(response) => response,
                Err(_) if attempt < self.options.max_retries => {
                    drop(permit);
                    self.defer(&state, retry_delay(attempt)).await;
                    continue;
                }
                Err(_) => return Err(FetchError::Transport),
            };
            let status = response.status();
            let headers = response.headers().clone();
            if headers
                .get("x-amzn-waf-action")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| matches!(v.trim(), "challenge" | "captcha"))
            {
                return Err(FetchError::Challenge(status.as_u16()));
            }
            if matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504) {
                let delay = headers
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after)
                    .unwrap_or_else(|| retry_delay(attempt));
                // A delay beyond this crawl's entire lifetime prevents new origin
                // requests too, rather than retrying sooner than Retry-After.
                self.defer(
                    &state,
                    delay.min(Duration::from_millis(
                        self.options.max_duration_ms.saturating_add(1),
                    )),
                )
                .await;
                if attempt < self.options.max_retries
                    && delay <= Duration::from_millis(self.options.max_retry_delay_ms)
                {
                    drop(permit);
                    continue;
                }
            }
            if !status.is_success() {
                return Ok(Raw {
                    status,
                    headers,
                    body: Vec::new(),
                });
            }
            if headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|len| len > body_limit as u64)
            {
                return Err(FetchError::Body);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Transport)? {
                self.bytes
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                        value
                            .checked_add(chunk.len())
                            .filter(|n| *n <= self.options.max_total_bytes)
                    })
                    .map_err(|_| FetchError::Bytes)?;
                if chunk.len() > body_limit.saturating_sub(body.len()) {
                    return Err(FetchError::Body);
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(Raw {
                status,
                headers,
                body,
            });
        }
        Err(FetchError::Transport)
    }
    pub(crate) async fn browser_resource(
        &self,
        request: &crate::browser::ResourceRequest,
    ) -> Result<Fetched, FetchError> {
        let url = normalize_url(&request.url, self.options.max_url_bytes)
            .map_err(|_| FetchError::Policy)?;
        if !self.permitted_resource(&url) || (request.navigation && !self.permitted(&url)) {
            return Err(FetchError::Policy);
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| FetchError::Policy)?;
        if !matches!(method, reqwest::Method::GET | reqwest::Method::HEAD) {
            if method != reqwest::Method::POST {
                return Err(FetchError::Policy);
            }
            let body = request.post_data.as_deref().ok_or(FetchError::Policy)?;
            if !self
                .options
                .browser
                .read_only_post_urls
                .iter()
                .any(|allowed| allowed == url.as_str())
            {
                if !self.options.browser.allow_graphql_queries {
                    return Err(FetchError::Policy);
                }
                match crate::browser::graphql_policy(body) {
                    crate::browser::GraphqlPolicy::Query => {}
                    crate::browser::GraphqlPolicy::NeedsDocument => {
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            "content-type",
                            reqwest::header::HeaderValue::from_static("application/json"),
                        );
                        headers.insert(
                            "x-web-fast-synthetic",
                            reqwest::header::HeaderValue::from_static("1"),
                        );
                        return Ok(Fetched { final_url: url, redirects: Vec::new(), headers, body: br#"{"errors":[{"message":"PersistedQueryNotFound","extensions":{"code":"PERSISTED_QUERY_NOT_FOUND"}}]}"#.to_vec(), status: 200 });
                    }
                    crate::browser::GraphqlPolicy::Blocked => return Err(FetchError::Policy),
                }
            }
        }
        if !self.robots(&url).await?.allows(&self.options.agent, &url) {
            return Err(FetchError::Robots);
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "accept"
                    | "content-type"
                    | "origin"
                    | "referer"
                    | "authorization"
                    | "x-shopify-storefront-access-token"
                    | "x-api-key"
                    | "x-requested-with"
            ) {
                if let (Ok(name), Ok(value)) = (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                    reqwest::header::HeaderValue::from_str(value),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        let raw = self
            .request_with(
                &url,
                self.options.max_response_bytes,
                method,
                headers,
                request.post_data.clone(),
            )
            .await?;
        Ok(Fetched {
            final_url: url,
            redirects: Vec::new(),
            headers: raw.headers,
            body: raw.body,
            status: raw.status.as_u16(),
        })
    }
    async fn defer(&self, state: &OriginState, duration: Duration) {
        let mut next = state.next_start.lock().await;
        *next = (*next).max(Instant::now() + duration);
    }
}
fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}
fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250u64.saturating_mul(1u64 << attempt.min(6)))
}
fn retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|time| time.duration_since(SystemTime::now()).unwrap_or_default())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn origin_pacing_spaces_concurrent_starts_without_catch_up_bursts() {
        let begin = Instant::now();
        let interval = Duration::from_millis(20);
        let state = OriginState {
            permits: Arc::new(Semaphore::new(3)),
            next_start: AsyncMutex::new(begin + Duration::from_millis(100)),
            robots: OnceCell::new(),
        };
        let start = || async {
            state.wait_start(interval).await;
            Instant::now()
        };
        let (a, b, c) = tokio::join!(start(), start(), start());
        let mut starts = [a, b, c];
        starts.sort();
        assert!(starts[0].duration_since(begin) >= Duration::from_millis(100));
        for pair in starts.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= interval);
        }
        // A stalled runtime must not accumulate credits for a burst.
        tokio::time::advance(Duration::from_secs(1)).await;
        let first = start().await;
        let second = start().await;
        assert!(second.duration_since(first) >= interval);
    }

    #[test]
    fn network_ranges_and_url_identity() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(
            normalize_url("HTTP://Example.COM:80/a/%7e?q=2&q=1#x", 1000)
                .unwrap()
                .as_str(),
            "http://example.com/a/~?q=2&q=1"
        );
        assert!(normalize_url("http://user:pass@example.com/", 1000).is_err());
    }
    #[test]
    fn robots_longest_match_groups_and_percent_encoding() {
        let rules = RobotsPolicy { body: "User-agent: *\nDisallow: /private\nAllow: /private/public$\nUser-agent: Special\nDisallow: /special\n".into(), deny_all: false, sitemaps: Vec::new() };
        assert!(!rules.allows(
            "web-fast-crawl",
            &Url::parse("https://e.test/private").unwrap()
        ));
        assert!(rules.allows(
            "web-fast-crawl",
            &Url::parse("https://e.test/private/public").unwrap()
        ));
        assert!(!rules.allows(
            "web-fast-crawl",
            &Url::parse("https://e.test/private/public/x").unwrap()
        ));
        assert!(rules.allows("Special", &Url::parse("https://e.test/private").unwrap()));
    }
}

#[cfg(test)]
mod dns_tests {
    use super::*;
    #[tokio::test]
    async fn actual_dns_answers_are_checked_before_connection() {
        let denied = GuardedDns {
            allow_private: false,
        }
        .resolve("localhost".parse().unwrap())
        .await;
        assert!(denied.is_err());
        let allowed = GuardedDns {
            allow_private: true,
        }
        .resolve("localhost".parse().unwrap())
        .await
        .unwrap();
        assert!(allowed
            .into_iter()
            .any(|address| address.ip().is_loopback()));
    }
}
