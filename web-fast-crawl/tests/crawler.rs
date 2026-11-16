mod support;
use std::sync::atomic::Ordering;
use support::{Response, Server};
use web_fast_crawl::{
    graph::Term,
    semantic::{LINK, REDIRECT},
    CrawlOptions, Crawler,
};

#[tokio::test]
async fn waf_challenge_is_not_successful_content_or_retried() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        _ => Response::status(202).header("x-amzn-waf-action", "challenge"),
    })
    .await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    let page = &report.pages[0];
    assert_eq!(page.status, Some(202));
    assert!(page
        .diagnostics
        .iter()
        .any(|d| d == "website access challenge (HTTP 202)"));
    assert!(page.document.is_none() && page.structured.is_none());
    assert!(report.graph.edges().is_empty());
    assert_eq!(server.count("/"), 1);

    let server =
        Server::start(|_, _, _| Response::status(202).header("x-amzn-waf-action", "captcha")).await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert!(report.pages[0]
        .diagnostics
        .iter()
        .any(|d| d.contains("robots")));
    assert_eq!(server.count("/"), 0);
}

#[tokio::test]
async fn crawl_deduplicates_urls_preserves_graph_and_obeys_robots_and_nofollow() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::html("User-agent: *\nDisallow: /private\n"),
        "/" => Response::html("<title>Home</title><h1>Root</h1><a href='/a#one'>A</a><a href='/a#two'>Again</a><a href='/private'>Private</a><a href='/nofollow' rel=nofollow>Skip</a><a href='https://outside.test/'>Outside</a>"),
        "/a" => Response::html("<title>A</title><a href='/'>Cycle</a>"),
        _ => Response::status(500),
    }).await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "complete");
    assert_eq!(server.count("/robots.txt"), 1);
    assert_eq!(server.count("/a"), 1);
    assert_eq!(server.count("/private"), 0);
    assert_eq!(server.count("/nofollow"), 0);
    assert!(
        report
            .pages
            .iter()
            .any(|p| p.url.ends_with("/private")
                && p.diagnostics.iter().any(|d| d.contains("robots")))
    );
    let root = report.graph.find(&Term::iri(server.url("/"))).unwrap();
    let predicate = report.graph.find(&Term::iri(LINK)).unwrap();
    assert_eq!(
        report
            .graph
            .outgoing(root)
            .filter(|edge| edge.predicate == predicate)
            .count(),
        4
    );
}
#[tokio::test]
async fn redirects_are_scoped_loop_checked_and_recorded_as_edges() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::status(302).header("Location", "/final"),
        "/final" => {
            Response::html("<p>Final</p><a href='/loop'>Loop</a><a href='/outside'>Outside</a>")
        }
        "/loop" => Response::status(302).header("Location", "/loop"),
        "/outside" => Response::status(302).header("Location", "http://127.0.0.1:9/private"),
        _ => Response::status(500),
    })
    .await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert!(report.graph.find(&Term::iri(REDIRECT)).is_some());
    assert_eq!(
        report.pages[0].final_url.as_deref(),
        Some(server.url("/final").as_str())
    );
    assert!(report
        .pages
        .iter()
        .any(|p| p.url.ends_with("/loop") && p.diagnostics.iter().any(|d| d.contains("redirect"))));
    assert!(
        report
            .pages
            .iter()
            .any(|p| p.url.ends_with("/outside")
                && p.diagnostics.iter().any(|d| d.contains("policy")))
    );
    assert_eq!(server.count("/loop"), 1);
}
#[tokio::test]
async fn noindex_drops_content_and_header_nofollow_stops_discovery() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::html("<meta name=robots content=noindex><title>Secret title</title><p>Secret content</p><a href='/child'>Child</a>").header("X-Robots-Tag", "otherbot: index, web-fast-crawl: nofollow"),
        _ => Response::status(500),
    }).await;
    let mut options = server.options();
    options.retain_documents = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert!(report.pages[0].robots.noindex && report.pages[0].robots.nofollow);
    assert!(report.pages[0].document.is_none() && report.pages[0].title.is_none());
    assert_eq!(report.graph.edges().len(), 0);
    assert_eq!(server.count("/child"), 0);
    assert!(!serde_json::to_string(&report).unwrap().contains("Secret"));
}
#[tokio::test]
async fn concurrency_and_pacing_bound_real_request_starts() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            let mut r = Response::html("<p>OK</p>");
            r.delay_ms = 80;
            r
        }
    })
    .await;
    let mut options = server.options();
    options.min_origin_interval_ms = 20;
    options.per_origin_concurrency = 2;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[
            server.url("/a"),
            server.url("/b"),
            server.url("/c"),
            server.url("/d"),
        ])
        .await
        .unwrap();
    assert_eq!(report.pages.len(), 4);
    assert_eq!(server.count("/robots.txt"), 1);
    assert_eq!(server.max_active.load(Ordering::SeqCst), 2);
    let hits = server.hits.lock().unwrap();
    for pair in hits.windows(2) {
        assert!(pair[1].1.duration_since(pair[0].1).as_millis() >= 15);
    }
}
#[tokio::test]
async fn retries_obey_retry_after_and_attempt_budget() {
    let server = Server::start(|path, count, _| match path {
        "/robots.txt" => Response::status(404),
        "/" if count == 1 => Response::status(503).header("Retry-After", "1"),
        _ => Response::html("<p>Recovered</p>"),
    })
    .await;
    let mut options = server.options();
    options.max_retries = 1;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert!(report.pages[0].diagnostics.is_empty());
    assert_eq!(report.requests, 3);
    let hits = server.hits.lock().unwrap();
    let page_hits = hits
        .iter()
        .filter(|(path, _)| path == "/")
        .collect::<Vec<_>>();
    assert!(page_hits[1].1.duration_since(page_hits[0].1).as_millis() >= 950);
}
#[tokio::test]
async fn request_byte_page_and_depth_budgets_stop_cleanly() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            Response::html("<p>Payload</p><a href='/child'>Child</a>")
        }
    })
    .await;
    let mut options = server.options();
    options.max_requests = 1;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "request_limit");
    assert_eq!(report.requests, 1);
    let mut options = server.options();
    options.max_total_bytes = 3;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "byte_limit");
    assert!(report.response_bytes <= 3);
    let mut options = server.options();
    options.max_pages = 1;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "page_limit");
    assert_eq!(report.pages.len(), 1);
    let mut options = server.options();
    options.max_depth = 0;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "complete");
    assert_eq!(report.pages.len(), 1);
}
#[tokio::test]
async fn oversized_compressed_responses_are_checked_after_decompression() {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(format!("<p>{}</p>", "x".repeat(10_000)).as_bytes())
        .unwrap();
    let body = encoder.finish().unwrap();
    let server = Server::start(move |path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            let mut response = Response::html("").header("Content-Encoding", "gzip");
            response.body = body.clone();
            response
        }
    })
    .await;
    let mut options = server.options();
    options.max_response_bytes = 512;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert!(report.pages[0]
        .diagnostics
        .iter()
        .any(|d| d.contains("response exceeds")));
    assert!(report.graph.edges().is_empty());
}
#[tokio::test]
async fn robots_unreachable_is_closed_but_404_is_open() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(503)
        } else {
            Response::html("<p>Should not be fetched</p>")
        }
    })
    .await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(server.count("/"), 0);
    assert!(report.pages[0].diagnostics[0].contains("robots"));
}
#[tokio::test]
async fn sitemap_indexes_discover_urls_and_deduplicate_cycles() {
    let server = Server::start(|path, _, base| match path {
        "/robots.txt" => Response::html(format!("User-agent: *\nSitemap: {base}/sitemap.xml\n")),
        "/sitemap.xml" => { let mut r = Response::html(format!("<sitemapindex><sitemap><loc>{base}/urls.xml</loc></sitemap><sitemap><loc>{base}/sitemap.xml</loc></sitemap></sitemapindex>")); r.mime = "application/xml".into(); r },
        "/urls.xml" => { let mut r = Response::html(format!("<urlset xmlns='http://www.sitemaps.org/schemas/sitemap/0.9'><url><loc>{base}/found?a=1&amp;b=2</loc></url></urlset>")); r.mime = "application/xml".into(); r },
        _ => Response::html("<p>Found</p>"),
    }).await;
    let mut options = server.options();
    options.discover_sitemaps = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "complete");
    assert_eq!(server.count("/sitemap.xml"), 1);
    assert_eq!(server.count("/found?a=1&b=2"), 1);
    assert_eq!(
        report.pages.iter().filter(|p| p.kind == "sitemap").count(),
        2
    );
}
#[tokio::test]
async fn semantic_links_and_focus_terms_affect_discovery() {
    let server = Server::start(|path, _, base| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::html(format!(r#"<a href='/other'>Other</a><a href='/rust'>Rust ontology</a><script type='application/ld+json'>{{"@context":{{"schema":"https://schema.org/"}},"@id":"{base}/entity","schema:url":{{"@id":"{base}/semantic"}}}}</script>"#)),
        _ => Response::html("<p>Result</p>"),
    }).await;
    let mut options = server.options();
    options.concurrency = 1;
    options.per_origin_concurrency = 1;
    options.max_pages = 2;
    options.focus_terms = vec!["ontology".into()];
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.pages[1].url, server.url("/rust"));
    assert_eq!(report.discovered_urls, 4);
}
#[tokio::test]
async fn charset_headers_bom_precedence_and_duplicate_content_are_preserved() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/legacy" => {
            let mut r = Response::html("");
            r.mime = "text/html; charset=windows-1252".into();
            r.body = b"<p>Caf\xe9</p>".to_vec();
            r
        }
        _ => {
            let mut r = Response::html("\u{feff}<p>Café</p>");
            r.mime = "text/html; charset=windows-1252".into();
            r
        }
    })
    .await;
    let mut options = server.options();
    options.retain_documents = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/legacy"), server.url("/a"), server.url("/b")])
        .await
        .unwrap();
    assert!(report
        .pages
        .iter()
        .all(|p| p.document.as_ref().unwrap().text == "Café"));
    assert_eq!(
        report.pages[2].duplicate_of.as_deref(),
        Some(server.url("/a").as_str())
    );
}
#[tokio::test]
async fn private_targets_require_opt_in_and_deadline_returns_partial_report() {
    assert!(Crawler::new(CrawlOptions::default())
        .unwrap()
        .crawl(&["http://127.0.0.1/".into()])
        .await
        .is_err());
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            let mut r = Response::html("<p>Slow</p>");
            r.delay_ms = 500;
            r
        }
    })
    .await;
    let mut options = server.options();
    options.max_duration_ms = 20;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "deadline");
}

#[tokio::test]
async fn frontier_and_graph_limits_are_reported_instead_of_silent_truncation() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            Response::html("<p>Body</p><a href='/a'>A</a><a href='/b'>B</a>")
        }
    })
    .await;
    let mut options = server.options();
    options.max_urls = 1;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "url_limit");
    assert_eq!(report.discovered_urls, 1);
    let mut options = server.options();
    options.graph.max_edges = 1;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "graph_limit");
    assert_eq!(report.graph.edges().len(), 1);
}
#[tokio::test]
async fn robots_redirects_and_page_redirect_targets_still_apply_rules() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(302).header("Location", "/rules"),
        "/rules" => Response::html("User-agent: *\nDisallow: /private\n"),
        "/" => Response::status(302).header("Location", "/private"),
        _ => Response::html("<p>Private</p>"),
    })
    .await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(server.count("/rules"), 1);
    assert_eq!(server.count("/private"), 0);
    assert!(report.pages[0].diagnostics[0].contains("robots"));
}
#[tokio::test]
async fn unsupported_media_and_html_parse_errors_are_page_failures() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/pdf" => {
            let mut r = Response::html("%PDF");
            r.mime = "application/pdf".into();
            r
        }
        _ => {
            let mut r = Response::html("");
            r.body = vec![0xff];
            r
        }
    })
    .await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/pdf"), server.url("/bad")])
        .await
        .unwrap();
    assert_eq!(report.pages[0].diagnostics, ["unsupported_content_type"]);
    assert!(report.pages[1].diagnostics[0].contains("encoding"));
    assert!(report.graph.edges().is_empty());
}
#[tokio::test]
async fn excessive_retry_after_defers_other_requests_until_deadline() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            Response::status(429).header("Retry-After", "9999999")
        }
    })
    .await;
    let mut options = server.options();
    options.concurrency = 1;
    options.per_origin_concurrency = 1;
    options.max_duration_ms = 60;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/a"), server.url("/b")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "deadline");
    assert_eq!(server.count("/a"), 1);
    assert_eq!(server.count("/b"), 0);
}
#[test]
fn invalid_runtime_configuration_is_rejected_before_fetching() {
    for options in [
        CrawlOptions {
            concurrency: 0,
            ..CrawlOptions::default()
        },
        CrawlOptions {
            request_timeout_ms: u64::MAX,
            ..CrawlOptions::default()
        },
        CrawlOptions {
            agent: "bot\r\ninjected".into(),
            ..CrawlOptions::default()
        },
    ] {
        assert!(Crawler::new(options).is_err());
    }
}

#[tokio::test]
async fn alternate_json_and_api_pagination_are_crawled_as_structured_data() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::html("<p>Catalog</p><link rel=alternate type=application/json href='/products.json'>"),
        "/products.json" => { let mut r = Response::html(r#"{"products":[{"title":"Shoe","sku":"s1","price":"12.30"}],"next":"/products.json?page=2"}"#); r.mime = "application/json".into(); r },
        _ => { let mut r = Response::html(r#"{"products":[{"title":"Hat","sku":"h1","price":"5.00"}],"next":null}"#); r.mime = "application/json".into(); r },
    }).await;
    let report = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.pages.len(), 3);
    assert_eq!(
        report.pages[1].structured.as_ref().unwrap().products[0]
            .name
            .as_deref(),
        Some("Shoe")
    );
    assert_eq!(
        report.pages[2].structured.as_ref().unwrap().products[0]
            .name
            .as_deref(),
        Some("Hat")
    );
    assert!(report.pages[1].document.is_none());
}

#[tokio::test]
async fn browser_valid_tracking_fragments_do_not_abort_rdf_projection() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        _ => Response::html("<title>Catalog</title><a href='/plans#origin=home#campaign=loyalty'>Plans</a><a href='/bad%escape'>Malformed escape</a><link rel=canonical href='/#one#two'>"),
    }).await;
    let mut options = server.options();
    options.max_depth = 0;
    options.retain_documents = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(report.stop_reason, "complete");
    let page = &report.pages[0];
    assert!(page.document.is_some());
    assert!(page
        .diagnostics
        .iter()
        .any(|code| code == "non_rdf_url_preserved_as_text"));
    assert!(report
        .graph
        .find(&web_fast_crawl::graph::Term::text(
            server.url("/plans#origin=home#campaign=loyalty")
        ))
        .is_some());
    assert!(report
        .graph
        .find(&web_fast_crawl::graph::Term::iri(server.url("/plans")))
        .is_some());
}

#[tokio::test]
async fn robots_block_at_redirect_is_distinguished_from_the_requested_page() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::html("User-agent: *\nDisallow: /verification\n"),
        "/product" => Response::status(302).header("Location", "/verification?next=product"),
        _ => Response::html("not fetched"),
    })
    .await;
    let r = Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/product")])
        .await
        .unwrap();
    assert_eq!(
        r.pages[0].diagnostics,
        ["redirect destination is disallowed by robots policy"]
    );
    assert_eq!(server.count("/product"), 1);
    assert_eq!(server.count("/verification?next=product"), 0);
}

#[tokio::test]
async fn static_requests_do_not_announce_the_internal_robots_token() {
    let server = Server::start(|path, _, _| {
        if path == "/robots.txt" {
            Response::status(404)
        } else {
            Response::html("<p>OK</p>")
        }
    })
    .await;
    Crawler::new(server.options())
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(server.user_agents.lock().unwrap().as_slice(), &[None, None]);
}

#[tokio::test]
async fn explicit_http_identity_is_independent_from_robots_policy_and_validated() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::html("User-agent: web-fast-crawl\nDisallow: /private\n"),
        _ => Response::html("<p>OK</p>"),
    })
    .await;
    let mut options = server.options();
    options.user_agent = Some("ExampleBrowser/1.0".into());
    let report = Crawler::new(options.clone())
        .unwrap()
        .crawl(&[server.url("/private")])
        .await
        .unwrap();
    assert_eq!(server.count("/private"), 0);
    assert_eq!(
        server.user_agents.lock().unwrap().as_slice(),
        &[Some("ExampleBrowser/1.0".into())]
    );
    assert_eq!(report.user_agent, options.user_agent);
    for bad in ["", "x\r\nInjected: yes", "x\tvalue"] {
        options.user_agent = Some(bad.into());
        assert!(Crawler::new(options.clone()).is_err());
    }
}
