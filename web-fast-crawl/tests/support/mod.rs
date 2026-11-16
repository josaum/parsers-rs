use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use web_fast_crawl::CrawlOptions;
#[derive(Clone)]
pub struct Response {
    pub status: u16,
    pub mime: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub delay_ms: u64,
}
impl Response {
    pub fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            mime: "text/html; charset=utf-8".into(),
            body: body.into().into_bytes(),
            headers: Vec::new(),
            delay_ms: 0,
        }
    }
    pub fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::html("")
        }
    }
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }
}
pub struct Server {
    pub base: String,
    pub hits: Arc<Mutex<Vec<(String, Instant)>>>,
    pub user_agents: Arc<Mutex<Vec<Option<String>>>>,
    pub max_active: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn start(
        handler: impl Fn(&str, usize, &str) -> Response + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let user_agents = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let task = {
            let (hits, max_active, base) = (hits.clone(), max_active.clone(), base.clone());
            let user_agents = user_agents.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let (hits, counts, active, max_active, handler, base) = (
                        hits.clone(),
                        counts.clone(),
                        active.clone(),
                        max_active.clone(),
                        handler.clone(),
                        base.clone(),
                    );
                    let user_agents = user_agents.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        loop {
                            let mut buf = [0; 2048];
                            let Ok(n) = socket.read(&mut buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            request.extend_from_slice(&buf[..n]);
                            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                                break;
                            }
                            if request.len() > 16384 {
                                return;
                            }
                        }
                        let request = String::from_utf8_lossy(&request);
                        user_agents.lock().unwrap().push(
                            request
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
                                .map(|(_, value)| value.trim().to_owned()),
                        );
                        let path = request
                            .lines()
                            .next()
                            .unwrap()
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .to_owned();
                        hits.lock().unwrap().push((path.clone(), Instant::now()));
                        let count = {
                            let mut counts = counts.lock().unwrap();
                            let count = counts.entry(path.clone()).or_insert(0);
                            *count += 1;
                            *count
                        };
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(current, Ordering::SeqCst);
                        let response = handler(&path, count, &base);
                        tokio::time::sleep(Duration::from_millis(response.delay_ms)).await;
                        let mut header = format!("HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", response.status, response.mime, response.body.len());
                        for (key, value) in response.headers {
                            header.push_str(&format!("{key}: {value}\r\n"));
                        }
                        header.push_str("\r\n");
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&response.body).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            })
        };
        Self {
            base,
            hits,
            user_agents,
            max_active,
            task,
        }
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
    pub fn count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }
    pub fn options(&self) -> CrawlOptions {
        CrawlOptions {
            allow_private_networks: true,
            min_origin_interval_ms: 0,
            max_retries: 0,
            discover_sitemaps: false,
            max_duration_ms: 5_000,
            ..CrawlOptions::default()
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
