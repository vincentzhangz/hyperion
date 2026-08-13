//! End-to-end browser tests: fetch a page from an in-process glommio server,
//! parse it, run its inline scripts, and query the result.

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;

use hyperion_browser::BrowserContext;
use hyperion_net::client::HttpClientOptions;
use hyperion_net::connector::GlommioStream;

const PAGE_HTML: &str = r#"<!DOCTYPE html>
<html>
  <head>
    <title>Hyperion Test Page</title>
  </head>
  <body>
    <h1 id="heading">Hello, Hyperion</h1>
    <p class="note">rendered by io_uring</p>
    <button id="btn">press</button>
    <input id="field" value="">
    <a id="go" href="/next">go</a>
    <script>
      globalThis.greeting = "from v8";
      globalThis.pairs = 21 * 2;
      document.querySelector('#btn').addEventListener('click', function () {
        document.querySelector('#heading').textContent = 'clicked';
      });
      document.querySelector('#btn').addEventListener('mouseover', function () {
        globalThis.hovered = true;
      });
      document.querySelector('#field').addEventListener('input', function () {
        globalThis.typed = this.value;
      });
    </script>
  </body>
</html>"#;

/// Serve `PAGE_HTML` on 127.0.0.1:0 in the background and return the addr.
fn serve_page() -> std::net::SocketAddr {
    let listener = glommio::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    glommio::spawn_local(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                break;
            };
            glommio::spawn_local(async move {
                let io = GlommioStream::new(stream);
                let svc = hyper::service::service_fn(
                    |_req: http::Request<hyper::body::Incoming>| async move {
                        Ok::<_, std::io::Error>(
                            http::Response::builder()
                                .status(http::StatusCode::OK)
                                .header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")
                                .body(Full::new(Bytes::from(PAGE_HTML)))
                                .unwrap(),
                        )
                    },
                );
                if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                    eprintln!("server conn error: {e}");
                }
            })
            .detach();
        }
    })
    .detach();
    addr
}

/// Run `body` inside a fresh glommio executor with the test page served.
/// `body` receives the loaded `Page` and returns a `Send` value; `Page`
/// itself is not `Send` (V8 isolate wrapper), so all assertions on the page
/// happen inside the executor.
fn with_page<R>(body: impl FnOnce(hyperion_browser::Page) -> R + Send + 'static) -> R
where
    R: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .spawn(move || async move {
            let addr = serve_page();
            let context = BrowserContext::new(HttpClientOptions::default());
            let page = context
                .navigate(&format!("http://{addr}/"))
                .await
                .expect("navigate should succeed");
            body(page)
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn navigate_parses_title_and_text() {
    with_page(|page| {
        assert_eq!(page.status(), 200);
        assert_eq!(page.title(), "Hyperion Test Page");
        let text = page.text_content();
        assert!(text.contains("Hello, Hyperion"), "text: {text}");
        assert!(text.contains("rendered by io_uring"), "text: {text}");
        assert!(
            text.contains("from v8"),
            "inline script should have run: {text}"
        );
    });
}

#[test]
fn dom_query_and_js_round_trip() {
    with_page(|page| {
        let heading = page
            .dom()
            .query_selector("h1#heading")
            .expect("valid selector")
            .expect("h1 should exist");
        assert_eq!(page.element_text(heading), "Hello, Hyperion");

        let notes = page
            .dom()
            .query_selector_all("p.note")
            .expect("valid selector");
        assert_eq!(notes.len(), 1);

        assert_eq!(page.evaluate_js("globalThis.pairs").unwrap(), "42");
        assert_eq!(page.evaluate_js("globalThis.greeting").unwrap(), "from v8");
        assert_eq!(
            page.evaluate_js("document.title").unwrap(),
            "Hyperion Test Page"
        );
        assert_eq!(
            page.evaluate_js("document.querySelector('#heading').textContent")
                .unwrap(),
            "Hello, Hyperion"
        );
    });
}

#[test]
fn register_op_is_callable_from_page_js() {
    with_page(|page| {
        page.register_op(7, |arg| {
            let n: i64 = arg.trim().parse().unwrap_or(0);
            format!(r#"{{"square":{}}}"#, n * n)
        });
        let out = page
            .evaluate_js(r#"JSON.parse(__hyperion_op(7, "9")).square"#)
            .unwrap();
        assert_eq!(out, "81");
    });
}

#[test]
fn js_dom_mutation_updates_snapshot() {
    with_page(|page| {
        page.evaluate_js("document.querySelector('#heading').textContent = 'mutated'")
            .unwrap();
        assert!(
            page.snapshot_html().contains("mutated"),
            "snapshot should reflect JS mutation: {}",
            page.snapshot_html()
        );
        assert_eq!(
            page.evaluate_js("document.querySelector('#heading').textContent")
                .unwrap(),
            "mutated"
        );
    });
}

#[test]
fn snapshot_html_annotates_stable_node_ids() {
    with_page(|page| {
        let heading = page
            .dom()
            .query_selector("h1#heading")
            .unwrap()
            .expect("heading");
        let id = page.node_id_of(heading);
        let snap = page.snapshot_html();
        assert!(
            snap.contains(&format!("data-hyperion-node-id=\"{id}\"")),
            "annotated snapshot missing id {id}: {snap}"
        );
        page.evaluate_js("document.querySelector('#heading').textContent = 'still-me'")
            .unwrap();
        let snap2 = page.snapshot_html();
        assert!(
            snap2.contains(&format!("data-hyperion-node-id=\"{id}\"")),
            "id must remain stable across text mutation: {snap2}"
        );
        page.evaluate_js(
            "document.body.appendChild(document.createElement('aside')).textContent='n'",
        )
        .unwrap();
        let aside = page.dom().query_selector("aside").unwrap().expect("aside");
        let aside_id = page.node_id_of(aside);
        assert_ne!(aside_id, id);
        let snap3 = page.snapshot_html();
        assert!(
            snap3.contains(&format!("data-hyperion-node-id=\"{aside_id}\"")),
            "new nodes get ids: {snap3}"
        );
    });
}

#[test]
fn node_interactions_dispatch_and_navigate() {
    with_page(|page| {
        let btn = page.dom().query_selector("#btn").unwrap().unwrap();
        let btn_id = page.node_id_of(btn);
        assert_eq!(
            page.dispatch_hover(btn_id).unwrap(),
            hyperion_browser::InteractionOutcome::Applied
        );
        assert_eq!(
            page.evaluate_js("String(globalThis.hovered)").unwrap(),
            "true"
        );
        assert_eq!(
            page.dispatch_click(btn_id).unwrap(),
            hyperion_browser::InteractionOutcome::Applied
        );
        assert_eq!(
            page.evaluate_js("document.querySelector('#heading').textContent")
                .unwrap(),
            "clicked"
        );

        let field = page.dom().query_selector("#field").unwrap().unwrap();
        let field_id = page.node_id_of(field);
        page.dispatch_text(Some(field_id), "hi").unwrap();
        assert_eq!(
            page.evaluate_js("document.querySelector('#field').value")
                .unwrap(),
            "hi"
        );
        assert_eq!(page.evaluate_js("String(globalThis.typed)").unwrap(), "hi");
        assert_eq!(
            page.evaluate_js("document.activeElement.id").unwrap(),
            "field"
        );

        let link = page.dom().query_selector("#go").unwrap().unwrap();
        let link_id = page.node_id_of(link);
        match page.dispatch_click(link_id).unwrap() {
            hyperion_browser::InteractionOutcome::Navigate(url) => {
                assert!(url.ends_with("/next"), "href resolved: {url}");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
        assert_eq!(
            page.dispatch_click(9_999_999).unwrap(),
            hyperion_browser::InteractionOutcome::Stale
        );
    });
}
