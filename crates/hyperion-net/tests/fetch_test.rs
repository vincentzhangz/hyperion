//! End-to-end tests: an in-process glommio HTTP server serving fixture pages,
//! exercised through `HttpClient::fetch`.

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use url::Url;

use hyperion_net::client::{HttpClient, HttpClientOptions};
use hyperion_net::connector::GlommioStream;

#[derive(Clone, Default)]
struct TestServer {
    cookie_hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for TestServer {
    type Response = http::Response<Full<Bytes>>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let path = req.uri().path().to_owned();
            let sent_cookie = req
                .headers()
                .get(http::header::COOKIE)
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if !sent_cookie.is_empty() {
                this.cookie_hits.lock().unwrap().push(sent_cookie.clone());
            }

            match path.as_str() {
                "/" => {
                    let body =
                        format!("hello from hyperion test server; sent-cookie={sent_cookie}");
                    Ok(http::Response::builder()
                        .status(http::StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/plain")
                        .body(Full::new(Bytes::from(body)))
                        .unwrap())
                }
                "/set-cookie" => {
                    let mut r = http::Response::builder()
                        .status(http::StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/plain")
                        .body(Full::new(Bytes::from("cookie set")))
                        .unwrap();
                    r.headers_mut().insert(
                        http::header::SET_COOKIE,
                        http::header::HeaderValue::from_static("session=abc123; Path=/"),
                    );
                    Ok(r)
                }
                "/redirect" => {
                    let mut r = http::Response::builder()
                        .status(http::StatusCode::FOUND)
                        .header(http::header::LOCATION, "/")
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                    r.headers_mut().insert(
                        http::header::SET_COOKIE,
                        http::header::HeaderValue::from_static("redirect_cookie=yes; Path=/"),
                    );
                    Ok(r)
                }
                other => {
                    let body = format!("no route: {other}");
                    Ok(http::Response::builder()
                        .status(http::StatusCode::NOT_FOUND)
                        .header(http::header::CONTENT_TYPE, "text/plain")
                        .body(Full::new(Bytes::from(body)))
                        .unwrap())
                }
            }
        })
    }
}

/// Run `body` inside a fresh glommio executor with an in-process test server
/// listening on `127.0.0.1:0`. Server and client share the same executor so the
/// `!Send` glommio sockets stay on one thread.
fn with_server<R, Fut>(body: impl FnOnce(std::net::SocketAddr) -> Fut + Send + 'static) -> R
where
    Fut: std::future::Future<Output = R> + 'static,
    R: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .spawn(move || async move {
            let server = TestServer::default();
            let listener = glommio::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            glommio::spawn_local(async move {
                loop {
                    let stream = listener.accept().await.unwrap();
                    let server = server.clone();
                    glommio::spawn_local(async move {
                        let io = GlommioStream::new(stream);
                        if let Err(e) = http1::Builder::new().serve_connection(io, server).await {
                            eprintln!("server conn error: {e}");
                        }
                    })
                    .detach();
                }
            })
            .detach();
            body(addr).await
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn fetch_returns_body() {
    with_server(|addr| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let url = format!("http://{addr}/");
        let resp = client.fetch(&url).await.expect("fetch should succeed");
        assert_eq!(resp.status, http::StatusCode::OK);
        let text = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(
            text.starts_with("hello from hyperion"),
            "unexpected body: {text}"
        );
    });
}

#[test]
fn fetch_sends_and_records_cookies() {
    with_server(|addr| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let base = Url::parse(&format!("http://{addr}")).unwrap();

        let resp = client
            .fetch(&format!("http://{addr}/set-cookie"))
            .await
            .expect("fetch /set-cookie");
        assert_eq!(resp.status, http::StatusCode::OK);
        assert!(
            client
                .cookies()
                .get(&base)
                .iter()
                .any(|(n, v)| n == "session" && v == "abc123"),
            "session cookie missing after set-cookie"
        );

        let resp = client
            .fetch(&format!("http://{addr}/"))
            .await
            .expect("fetch /");
        let text = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(
            text.contains("session=abc123"),
            "cookie not sent on second request: {text}"
        );
    });
}

#[test]
fn fetch_follows_redirects() {
    with_server(|addr| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let url = format!("http://{addr}/redirect");
        let resp = client.fetch(&url).await.expect("fetch should succeed");
        assert_eq!(resp.status, http::StatusCode::OK);
        assert_eq!(resp.url, format!("http://{addr}/"));

        let base = Url::parse(&format!("http://{addr}")).unwrap();
        assert!(
            client
                .cookies()
                .get(&base)
                .iter()
                .any(|(n, v)| n == "redirect_cookie" && v == "yes"),
            "redirect Set-Cookie not recorded"
        );
    });
}

#[test]
fn fetch_not_found() {
    with_server(|addr| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let resp = client
            .fetch(&format!("http://{addr}/missing"))
            .await
            .expect("fetch should succeed");
        assert_eq!(resp.status, http::StatusCode::NOT_FOUND);
        let text = String::from_utf8(resp.body.to_vec()).unwrap();
        assert_eq!(text, "no route: /missing");
    });
}

#[test]
fn fetch_emits_network_events() {
    with_server(|addr| async move {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let options = HttpClientOptions {
            net_events: Some(std::sync::Arc::new(move |e| {
                events_clone.lock().unwrap().push(e);
            })),
            ..Default::default()
        };

        let client = HttpClient::new(options);
        client
            .fetch(&format!("http://{addr}/"))
            .await
            .expect("fetch should succeed");

        let events = events.lock().unwrap();
        assert!(!events.is_empty());
        assert!(matches!(
            &events[0],
            hyperion_net::client::NetEvent::Request { id: 1, .. }
        ));
        assert!(matches!(
            &events[1],
            hyperion_net::client::NetEvent::Response { status: 200, .. }
        ));
        assert!(matches!(
            &events[2],
            hyperion_net::client::NetEvent::Finished { .. }
        ));
    });
}

#[test]
fn fetch_follows_redirects_and_emits_per_request() {
    with_server(|addr| async move {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let options = HttpClientOptions {
            net_events: Some(std::sync::Arc::new(move |e| {
                events_clone.lock().unwrap().push(e);
            })),
            ..Default::default()
        };

        let client = HttpClient::new(options);
        let resp = client
            .fetch(&format!("http://{addr}/redirect"))
            .await
            .expect("fetch should succeed");
        assert_eq!(resp.url, format!("http://{addr}/"));

        let events = events.lock().unwrap();
        let requests: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                hyperion_net::client::NetEvent::Request { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        // Redirect chain: initial 3xx + the followed request.
        assert_eq!(requests, vec![1, 2], "two distinct requests: {requests:?}");
        assert!(events.iter().any(|e| {
            matches!(
                e,
                hyperion_net::client::NetEvent::Response { status: 302, .. }
            )
        }));
    });
}

#[test]
fn fetch_intercept_fulfill_skips_network() {
    use std::rc::Rc;

    use hyperion_net::client::{FetchAction, FetchIntercept};

    with_server(|addr| async move {
        let intercept = FetchIntercept::new();
        intercept.set_enabled(true, Vec::new());
        let paused: Rc<std::cell::RefCell<Option<u64>>> = Rc::new(std::cell::RefCell::new(None));
        let paused_flag = paused.clone();
        let intercept_for_sink = intercept.clone();
        let options = HttpClientOptions {
            fetch_intercept: Some(intercept.clone()),
            fetch_paused: Some(Rc::new(move |id, _url, _method, _headers| {
                *paused_flag.borrow_mut() = Some(id);
                intercept_for_sink.provide_decision(
                    &id.to_string(),
                    FetchAction::Fulfill {
                        status: 200,
                        headers: vec![("content-type".into(), "text/plain".into())],
                        body: Bytes::from_static(b"fulfilled-body"),
                    },
                );
            })),
            ..Default::default()
        };
        let client = HttpClient::new(options);
        let resp = client
            .fetch(&format!("http://{addr}/"))
            .await
            .expect("fulfilled fetch");
        assert_eq!(resp.status.as_u16(), 200);
        assert_eq!(&resp.body[..], b"fulfilled-body");
        assert!(paused.borrow().is_some(), "pause sink should have fired");
    });
}
