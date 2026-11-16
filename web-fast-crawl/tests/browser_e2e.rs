#[allow(dead_code)] // Shared fixture helpers also used by the HTTP-only suite.
mod support;
use support::{Response, Server};
use web_fast_crawl::{browser::RenderMode, Crawler};

#[tokio::test]
#[ignore = "requires npm ci in browser/ and Chrome/Chromium; exercised by browser CI"]
async fn javascript_api_json_shadow_dom_and_read_only_graphql_work_end_to_end() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::html(r#"<html><head><title>JS shop</title></head><body><div id='root'>Loading</div><shop-card></shop-card><script>
          fetch('/api/products').then(r=>r.json()).then(data=>{
            data.browser_user_agent=navigator.userAgent;
            window.__INITIAL_STATE__=data;
            document.getElementById('root').innerHTML='<h1>'+data.products[0].name+'</h1>';
            document.querySelector('shop-card').attachShadow({mode:'open'}).innerHTML='<p>Shadow offer</p>';
          });
          fetch('/graphql', {method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({query:'query Catalog { products { name } }'})}).then(r=>r.json());
          fetch('/mutation', {method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({query:'mutation Buy { purchase { id } }'})}).catch(()=>{});
        </script></body></html>"#),
        "/api/products" => { let mut r = Response::html(r#"{"products":[{"name":"Rendered Shoe","sku":"S1","price":"12.30","currency":"BRL"}]}"#); r.mime = "application/json".into(); r },
        "/graphql" => { let mut r = Response::html(r#"{"data":{"products":[{"name":"GraphQL Hat","sku":"H1","price":"5.00"}]}}"#); r.mime = "application/json".into(); r },
        _ => Response::status(404),
    }).await;
    let mut options = server.options();
    options.browser.mode = RenderMode::Always;
    options.browser.channel = "chromium".into();
    options.browser.executable_path = std::env::var("WEB_FAST_BROWSER_EXECUTABLE").ok();
    options.browser.scroll_steps = 0;
    options.browser.settle_ms = 750;
    options.browser.dom_quiet_ms = 50;
    options.max_duration_ms = 20_000;
    options.max_depth = 0;
    options.retain_documents = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    let page = &report.pages[0];
    assert!(page.rendered_url.is_some(), "{:?}", page.diagnostics);
    assert!(page
        .document
        .as_ref()
        .unwrap()
        .text
        .contains("Rendered Shoe"));
    assert!(page
        .document
        .as_ref()
        .unwrap()
        .text
        .contains("Shadow offer"));
    let structured = page.structured.as_ref().unwrap();
    assert!(structured
        .sources
        .iter()
        .any(|source| source.kind == "network_json"));
    assert!(structured
        .sources
        .iter()
        .any(|source| source.kind == "browser_state"));
    assert!(structured
        .products
        .iter()
        .any(|product| product.name.as_deref() == Some("GraphQL Hat")));
    let identity = report.user_agent.as_deref().unwrap();
    assert!(!identity.contains("web-fast-crawl"));
    assert!(identity.contains("Mozilla/"));
    let state = structured
        .sources
        .iter()
        .find(|source| source.kind == "browser_state" && source.locator == "__INITIAL_STATE__")
        .unwrap();
    assert_eq!(state.value["browser_user_agent"].as_str(), Some(identity));
    assert!(server
        .user_agents
        .lock()
        .unwrap()
        .iter()
        .all(|value| value.as_deref() == Some(identity)));
    assert_eq!(server.count("/api/products"), 1);
    assert_eq!(server.count("/graphql"), 1);
    assert_eq!(server.count("/mutation"), 0);
    assert_eq!(
        server.count("/"),
        1,
        "initial HTML must be replayed, not fetched twice"
    );
}

#[tokio::test]
#[ignore = "requires npm ci in browser/ and Chrome/Chromium; exercised by browser CI"]
async fn javascript_navigation_preserves_response_robots_directives() {
    let server = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/" => Response::html("<script>setTimeout(()=>location.replace('/private'), 50)</script>"),
        "/private" => {
            Response::html("<title>Private</title><p>Do not index</p><a href='/next'>Next</a>")
                .header("X-Robots-Tag", "noindex, nofollow")
        }
        _ => Response::html("should not be visited"),
    })
    .await;
    let mut options = server.options();
    options.browser.mode = RenderMode::Always;
    options.browser.channel = "chromium".into();
    options.browser.executable_path = std::env::var("WEB_FAST_BROWSER_EXECUTABLE").ok();
    options.browser.scroll_steps = 0;
    options.browser.settle_ms = 1500;
    options.browser.dom_quiet_ms = 50;
    options.max_duration_ms = 20_000;
    options.retain_documents = true;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    let page = &report.pages[0];
    assert_eq!(
        page.rendered_url.as_deref(),
        Some(server.url("/private").as_str()),
        "{:?}",
        page.diagnostics
    );
    assert!(page.robots.noindex && page.robots.nofollow);
    assert!(page.document.is_none() && page.structured.is_none() && page.title.is_none());
    assert_eq!(server.count("/next"), 0);
}

#[tokio::test]
#[ignore = "requires npm ci in browser/ and Chrome/Chromium; exercised by browser CI"]
async fn resource_origins_do_not_expand_page_scope() {
    let resources = Server::start(|path, _, _| match path {
        "/robots.txt" => Response::status(404),
        "/catalog.js" => {
            let mut response = Response::html(
                "document.body.insertAdjacentHTML('beforeend', '<p>CDN catalog loaded</p>')",
            );
            response.mime = "text/javascript".into();
            response
        }
        _ => Response::html("out of page scope"),
    })
    .await;
    let denied = Server::start(|_, _, _| Response::html("not allowed")).await;
    let resource_base = resources.base.clone();
    let denied_base = denied.base.clone();
    let server = Server::start(move |path, _, _| match path {
        "/robots.txt" => Response::status(404),
        _ => Response::html(format!("<script src='{resource_base}/catalog.js'></script><script src='{denied_base}/blocked.js'></script><a href='{resource_base}/next'>Next</a><script>setTimeout(()=>location.assign('{resource_base}/navigate'), 100)</script>")),
    }).await;
    let mut options = server.options();
    options.browser.mode = RenderMode::Always;
    options.browser.channel = "chromium".into();
    options.browser.executable_path = std::env::var("WEB_FAST_BROWSER_EXECUTABLE").ok();
    options.browser.resource_origins = vec![resources.base.clone()];
    options.browser.scroll_steps = 0;
    options.browser.settle_ms = 1000;
    options.browser.dom_quiet_ms = 50;
    options.max_duration_ms = 20_000;
    let report = Crawler::new(options)
        .unwrap()
        .crawl(&[server.url("/")])
        .await
        .unwrap();
    assert_eq!(resources.count("/catalog.js"), 1);
    assert_eq!(resources.count("/next"), 0);
    assert_eq!(resources.count("/navigate"), 0);
    assert!(denied.hits.lock().unwrap().is_empty());
    assert_eq!(report.pages.len(), 1);
}
