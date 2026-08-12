//! End-to-end CDP tests: a real `CdpServer` on a glommio executor, exercised
//! over plain HTTP (`/json/*`) and a hand-rolled masked WebSocket client.

use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use serde_json::{Value, json};
use sha1::Digest;

use hyperion_cdp::{CdpServer, CdpServerOptions};
use hyperion_net::client::{HttpClient, HttpClientOptions};
use hyperion_net::connector::GlommioStream;

const PAGE_HTML: &str = "<!doctype html><html><head><title>CDP Test</title></head><body><h1 id=\"main\">Hello</h1><p class=\"x\">one</p><p class=\"x\">two</p></body></html>";

/// A tiny in-process HTML server so navigations don't need the network.
struct HtmlServer;

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for HtmlServer {
    type Response = http::Response<Full<Bytes>>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, _req: http::Request<hyper::body::Incoming>) -> Self::Future {
        Box::pin(async move {
            Ok(http::Response::builder()
                .status(http::StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/html")
                .body(Full::new(Bytes::from(PAGE_HTML)))
                .unwrap())
        })
    }
}

/// Run `body` on a fresh glommio executor with (a) a CDP server bound to an
/// ephemeral port and (b) an HTML server the tests navigate to.
fn with_cdp<F, Fut>(body: F)
where
    F: FnOnce(std::net::SocketAddr, std::net::SocketAddr) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    with_cdp_token(None, body);
}

fn with_cdp_token<F, Fut>(token: Option<String>, body: F)
where
    F: FnOnce(std::net::SocketAddr, std::net::SocketAddr) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    with_cdp_config(move |c| c.auth_token = token, body);
}

fn with_cdp_config<F, Fut, M>(mutate: M, body: F)
where
    M: FnOnce(&mut hyperion_config::Config) + Send + 'static,
    F: FnOnce(std::net::SocketAddr, std::net::SocketAddr) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .spawn(move || async move {
            let html_listener = glommio::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let html_addr = html_listener.local_addr().unwrap();
            glommio::spawn_local(async move {
                loop {
                    let stream = html_listener.accept().await.unwrap();
                    glommio::spawn_local(async move {
                        let io = GlommioStream::new(stream);
                        if let Err(e) = http1::Builder::new().serve_connection(io, HtmlServer).await
                        {
                            eprintln!("html server conn error: {e}");
                        }
                    })
                    .detach();
                }
            })
            .detach();

            let mut options = CdpServerOptions::default();
            mutate(&mut options.config);
            let server = CdpServer::new(options).unwrap();
            let cdp_addr = server.local_addr();
            glommio::spawn_local(server.run()).detach();

            body(cdp_addr, html_addr).await;
        })
        .unwrap()
        .join()
        .unwrap()
}

// ----------------------------------------------------------------------
// Minimal masked WebSocket client
// ----------------------------------------------------------------------

async fn ws_connect(addr: &str, path: &str) -> std::io::Result<(glommio::net::TcpStream, Vec<u8>)> {
    use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = glommio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof during handshake",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]);
            assert!(
                head.starts_with("HTTP/1.1 101"),
                "expected 101, got: {head}"
            );

            let mut hasher = sha1::Sha1::new();
            hasher.update(key.as_bytes());
            hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
            let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
            assert!(
                head.contains(&format!("Sec-WebSocket-Accept: {accept}")),
                "bad accept key in: {head}"
            );
            let leftover = buf[pos + 4..].to_vec();
            return Ok((stream, leftover));
        }
    }
}

/// Send a masked text frame.
async fn ws_send_text(stream: &mut glommio::net::TcpStream, payload: &str) -> std::io::Result<()> {
    use futures_lite::io::AsyncWriteExt;
    let mask = [0x12, 0x34, 0x56, 0x78];
    let data = payload.as_bytes();
    let mut frame = Vec::new();
    frame.push(0x81);
    let len = data.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= 0xffff {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (i, b) in data.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    stream.write_all(&frame).await
}

/// Read one unmasked server text frame as JSON, or `None` on timeout.
/// `leftover` holds bytes already read past the HTTP upgrade (may be empty).
async fn ws_read_frame(
    stream: &mut glommio::net::TcpStream,
    leftover: &mut Vec<u8>,
    timeout: Duration,
) -> Option<Value> {
    use futures_lite::io::AsyncReadExt;

    async fn read_exact(
        stream: &mut glommio::net::TcpStream,
        leftover: &mut Vec<u8>,
        buf: &mut [u8],
    ) -> Option<()> {
        let mut filled = 0;
        while filled < buf.len() {
            if !leftover.is_empty() {
                let n = (buf.len() - filled).min(leftover.len());
                buf[filled..filled + n].copy_from_slice(&leftover[..n]);
                leftover.drain(..n);
                filled += n;
                continue;
            }
            let n = stream.read(&mut buf[filled..]).await.ok()?;
            if n == 0 {
                return None;
            }
            filled += n;
        }
        Some(())
    }

    let read = Box::pin(async {
        let mut hdr = [0u8; 2];
        read_exact(stream, leftover, &mut hdr).await?;
        let opcode = hdr[0] & 0x0f;
        let mut len = u64::from(hdr[1] & 0x7f);
        if len == 126 {
            let mut b = [0u8; 2];
            read_exact(stream, leftover, &mut b).await?;
            len = u64::from(u16::from_be_bytes(b));
        } else if len == 127 {
            let mut b = [0u8; 8];
            read_exact(stream, leftover, &mut b).await?;
            len = u64::from_be_bytes(b);
        }
        let mut payload = vec![0u8; len as usize];
        read_exact(stream, leftover, &mut payload).await?;
        match opcode {
            0x1 => serde_json::from_slice::<Value>(&payload).ok(),
            _ => None,
        }
    });
    let timer = Box::pin(async {
        glommio::timer::Timer::new(timeout).await;
        None::<Value>
    });
    futures_lite::future::race(read, timer).await
}

/// Send a command and return the response for the same `id`.
async fn ws_command(
    stream: &mut glommio::net::TcpStream,
    leftover: &mut Vec<u8>,
    id: i64,
    method: &str,
    params: Option<Value>,
) -> Value {
    let mut msg = json!({ "id": id, "method": method });
    if let Some(p) = params {
        msg["params"] = p;
    }
    ws_send_text(stream, &msg.to_string()).await.unwrap();
    let deadline = Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        let frame = ws_read_frame(stream, leftover, deadline.saturating_sub(start.elapsed()))
            .await
            .unwrap_or_else(|| panic!("timeout waiting for response to {method}"));
        if frame.get("id") == Some(&json!(id)) {
            return frame;
        }
    }
}

/// Send a command, discarding event frames, then read frames until `method`
/// appears (ignoring responses) or `timeout` elapses.
async fn ws_expect_event(
    stream: &mut glommio::net::TcpStream,
    leftover: &mut Vec<u8>,
    method: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = ws_read_frame(stream, leftover, remaining).await?;
        if frame.get("method").and_then(Value::as_str) == Some(method) {
            return Some(frame);
        }
    }
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[test]
fn json_version() {
    with_cdp(|addr, _| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let resp = client
            .fetch(&format!("http://{addr}/json/version"))
            .await
            .unwrap();
        assert_eq!(resp.status.as_u16(), 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["Browser"], "hyperion/0.1");
        assert_eq!(v["Protocol-Version"], "1.3");
        assert!(v["V8-Version"].as_str().is_some());
        assert!(
            v["webSocketDebuggerUrl"]
                .as_str()
                .unwrap()
                .contains("/devtools/browser/")
        );
    });
}

#[test]
fn json_new_list_close() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");

        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        assert_eq!(resp.status.as_u16(), 200);
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(desc["type"], "page");
        let id = desc["id"].as_str().unwrap().to_owned();

        let resp = client
            .fetch(&format!("http://{addr}/json/list"))
            .await
            .unwrap();
        let list: Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(
            list.as_array().unwrap().iter().any(|t| t["id"] == id),
            "new target missing from /json/list: {list}"
        );

        let resp = client
            .fetch(&format!("http://{addr}/json/close/{id}"))
            .await
            .unwrap();
        let closed: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(closed, json!(true));

        let resp = client
            .fetch(&format!("http://{addr}/json/list"))
            .await
            .unwrap();
        let list: Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(
            !list.as_array().unwrap().iter().any(|t| t["id"] == id),
            "closed target still listed: {list}"
        );
    });
}

#[test]
fn ws_runtime_evaluate() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        let resp = ws_command(&mut ws, &mut ws_left, 1, "Runtime.enable", None).await;
        assert!(
            resp.get("result").is_some(),
            "Runtime.enable failed: {resp}"
        );

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            2,
            "Runtime.evaluate",
            Some(json!({ "expression": "1 + 1", "returnByValue": true })),
        )
        .await;
        assert_eq!(resp["result"]["result"]["type"], "number");
        assert_eq!(resp["result"]["result"]["value"], json!(2));

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            3,
            "Runtime.evaluate",
            Some(json!({ "expression": "'hello'", "returnByValue": true })),
        )
        .await;
        assert_eq!(resp["result"]["result"]["value"], json!("hello"));

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            4,
            "Runtime.evaluate",
            Some(json!({ "expression": "throw new Error('boom')" })),
        )
        .await;
        assert!(
            resp["result"].get("exceptionDetails").is_some(),
            "expected exceptionDetails: {resp}"
        );
        assert_eq!(resp["result"]["result"]["subtype"], "error");
    });
}

#[test]
fn ws_page_navigate_emits_events() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        // Page and Network events only flow once the domains are enabled.
        let resp = ws_command(&mut ws, &mut ws_left, 1, "Page.enable", None).await;
        assert!(resp.get("result").is_some(), "Page.enable failed: {resp}");
        let resp = ws_command(&mut ws, &mut ws_left, 2, "Network.enable", None).await;
        assert!(
            resp.get("result").is_some(),
            "Network.enable failed: {resp}"
        );

        let mut methods = Vec::new();
        ws_send_text(
            &mut ws,
            &json!({ "id": 3, "method": "Page.navigate", "params": { "url": page_url } })
                .to_string(),
        )
        .await
        .unwrap();
        let mut saw_load = false;
        for _ in 0..50 {
            let frame = ws_read_frame(&mut ws, &mut ws_left, Duration::from_secs(5))
                .await
                .expect("timed out waiting for navigation events");
            if let Some(m) = frame.get("method").and_then(Value::as_str) {
                methods.push(m.to_owned());
                if m == "Page.loadEventFired" {
                    saw_load = true;
                    break;
                }
            } else if frame.get("id") == Some(&json!(3)) {
                // navigate response; ignore
            }
        }
        assert!(saw_load, "no Page.loadEventFired; saw: {methods:?}");
        for expected in [
            "Page.frameNavigated",
            "Network.requestWillBeSent",
            "Network.responseReceived",
            "Network.loadingFinished",
        ] {
            assert!(
                methods.iter().any(|m| m == expected),
                "missing {expected}; saw: {methods:?}"
            );
        }
    });
}

#[test]
fn ws_dom_get_document_and_mutate() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        let resp = ws_command(&mut ws, &mut ws_left, 1, "DOM.enable", None).await;
        assert!(resp.get("result").is_some(), "DOM.enable failed: {resp}");

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            2,
            "DOM.getDocument",
            Some(json!({ "depth": -1 })),
        )
        .await;
        assert_eq!(resp["result"]["root"]["nodeId"], json!(1));
        assert_eq!(resp["result"]["root"]["nodeType"], json!(9));
        assert_eq!(resp["result"]["root"]["nodeName"], "#document");
        assert_eq!(resp["result"]["root"]["documentURL"], page_url);
        let children = resp["result"]["root"]["children"].as_array().unwrap();
        assert!(!children.is_empty(), "document has no children: {resp}");

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            3,
            "DOM.querySelector",
            Some(json!({ "nodeId": 1, "selector": "h1" })),
        )
        .await;
        let h1 = resp["result"]["nodeId"]
            .as_i64()
            .expect("h1 not found: {resp}");

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            4,
            "DOM.getOuterHTML",
            Some(json!({ "nodeId": h1 })),
        )
        .await;
        assert_eq!(
            resp["result"]["outerHTML"],
            json!(r#"<h1 id="main">Hello</h1>"#),
            "unexpected outerHTML: {resp}"
        );

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            5,
            "DOM.querySelectorAll",
            Some(json!({ "nodeId": 1, "selector": ".x" })),
        )
        .await;
        let xs = resp["result"]["nodeIds"].as_array().unwrap();
        assert_eq!(xs.len(), 2, "expected two .x nodes: {resp}");

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            6,
            "DOM.setAttribute",
            Some(json!({ "nodeId": h1, "name": "data-test", "value": "yes" })),
        )
        .await;
        assert!(resp.get("result").is_some(), "setAttribute failed: {resp}");
        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            7,
            "DOM.getOuterHTML",
            Some(json!({ "nodeId": h1 })),
        )
        .await;
        assert!(
            resp["result"]["outerHTML"]
                .as_str()
                .unwrap()
                .contains("data-test"),
            "attribute not set: {resp}"
        );

        // Mutations are visible to a fresh serialization.
        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            8,
            "DOM.querySelector",
            Some(json!({ "nodeId": 1, "selector": "p.x" })),
        )
        .await;
        let p = resp["result"]["nodeId"].as_i64().unwrap();
        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            9,
            "DOM.removeNode",
            Some(json!({ "nodeId": p })),
        )
        .await;
        assert!(resp.get("result").is_some(), "removeNode failed: {resp}");
        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            10,
            "DOM.querySelectorAll",
            Some(json!({ "nodeId": 1, "selector": ".x" })),
        )
        .await;
        assert_eq!(
            resp["result"]["nodeIds"].as_array().unwrap().len(),
            1,
            "removeNode did not drop a match: {resp}"
        );
    });
}

#[test]
fn ws_network_get_response_body() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        ws_command(&mut ws, &mut ws_left, 1, "Page.enable", None).await;
        ws_command(&mut ws, &mut ws_left, 2, "Network.enable", None).await;
        // Send navigate raw: ws_command would skip the loadingFinished event.
        ws_send_text(
            &mut ws,
            &json!({ "id": 3, "method": "Page.navigate", "params": { "url": page_url } })
                .to_string(),
        )
        .await
        .unwrap();

        let finished = ws_expect_event(
            &mut ws,
            &mut ws_left,
            "Network.loadingFinished",
            Duration::from_secs(5),
        )
        .await
        .expect("no Network.loadingFinished");
        let request_id = finished["params"]["requestId"].as_str().unwrap().to_owned();

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            4,
            "Network.getResponseBody",
            Some(json!({ "requestId": request_id })),
        )
        .await;
        assert_eq!(resp["result"]["body"], PAGE_HTML, "unexpected body: {resp}");
    });
}

#[test]
fn ws_runtime_console_and_log() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        // Page already loaded; enable Runtime and Log so the events flow.
        ws_command(&mut ws, &mut ws_left, 1, "Runtime.enable", None).await;
        ws_command(&mut ws, &mut ws_left, 2, "Log.enable", None).await;

        // Send evaluate raw so the consoleAPICalled event is not consumed by
        // the command's frame skip.
        ws_send_text(
            &mut ws,
            &json!({ "id": 3, "method": "Runtime.evaluate", "params": { "expression": "console.log('hello','world')" } })
                .to_string(),
        )
        .await
        .unwrap();

        let event = ws_expect_event(
            &mut ws,
            &mut ws_left,
            "Runtime.consoleAPICalled",
            Duration::from_secs(5),
        )
        .await
        .expect("no Runtime.consoleAPICalled");
        assert_eq!(event["params"]["type"], "log");
        let args: Vec<&str> = event["params"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["value"].as_str())
            .collect();
        assert_eq!(args, vec!["hello", "world"], "unexpected args: {event}");

        ws_send_text(
            &mut ws,
            &json!({ "id": 4, "method": "Runtime.evaluate", "params": { "expression": "console.error('kaboom')" } })
                .to_string(),
        )
        .await
        .unwrap();
        let entry = ws_expect_event(
            &mut ws,
            &mut ws_left,
            "Log.entryAdded",
            Duration::from_secs(5),
        )
        .await
        .expect("no Log.entryAdded");
        assert_eq!(entry["params"]["entry"]["level"], "error");
        assert_eq!(entry["params"]["entry"]["text"], "kaboom");
    });
}

#[test]
fn ws_runtime_evaluate_with_dom() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            1,
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.querySelector('#main').textContent",
                "returnByValue": true
            })),
        )
        .await;
        assert_eq!(resp["result"]["result"]["value"], json!("Hello"));

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            2,
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.querySelector('#main').textContent = 'from-js'; document.querySelector('#main').textContent",
                "returnByValue": true
            })),
        )
        .await;
        assert_eq!(resp["result"]["result"]["value"], json!("from-js"));

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            3,
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.querySelector('#main')",
                "returnByValue": false
            })),
        )
        .await;
        assert!(
            resp["result"]["result"].get("objectId").is_some(),
            "expected objectId for Element: {resp}"
        );
    });
}

#[test]
fn ws_cookies_round_trip() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();
        let host = html.ip().to_string();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            1,
            "Network.setCookie",
            Some(json!({
                "name": "session",
                "value": "abc",
                "domain": host,
                "path": "/"
            })),
        )
        .await;
        assert_eq!(resp["result"]["success"], json!(true), "setCookie: {resp}");

        let resp = ws_command(&mut ws, &mut ws_left, 2, "Network.getAllCookies", None).await;
        let cookies = resp["result"]["cookies"].as_array().unwrap();
        assert!(
            cookies
                .iter()
                .any(|c| c["name"] == "session" && c["value"] == "abc"),
            "cookie missing: {resp}"
        );

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            3,
            "Network.getCookies",
            Some(json!({ "urls": [page_url] })),
        )
        .await;
        assert!(
            resp["result"]["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "session"),
            "getCookies missing session: {resp}"
        );

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            4,
            "Network.deleteCookies",
            Some(json!({ "name": "session", "domain": host, "path": "/" })),
        )
        .await;
        assert!(resp.get("result").is_some(), "deleteCookies: {resp}");

        let resp = ws_command(&mut ws, &mut ws_left, 5, "Network.getAllCookies", None).await;
        assert!(
            !resp["result"]["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "session"),
            "cookie still present: {resp}"
        );
    });
}

#[test]
fn ws_fetch_fulfill_intercepts_navigation() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();

        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();

        ws_command(
            &mut ws,
            &mut ws_left,
            1,
            "Fetch.enable",
            Some(json!({ "patterns": [] })),
        )
        .await;
        ws_command(&mut ws, &mut ws_left, 2, "Page.enable", None).await;

        // Navigate without waiting so we can answer Fetch.requestPaused.
        ws_send_text(
            &mut ws,
            &json!({
                "id": 3,
                "method": "Page.navigate",
                "params": { "url": page_url }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let paused = ws_expect_event(
            &mut ws,
            &mut ws_left,
            "Fetch.requestPaused",
            Duration::from_secs(5),
        )
        .await
        .expect("no Fetch.requestPaused");
        let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();

        let body_html = "<!doctype html><html><head><title>Fulfilled</title></head><body><h1 id=\"main\">intercepted</h1></body></html>";
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(body_html.as_bytes());
        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            4,
            "Fetch.fulfillRequest",
            Some(json!({
                "requestId": request_id,
                "responseCode": 200,
                "responseHeaders": [{ "name": "content-type", "value": "text/html" }],
                "body": body_b64
            })),
        )
        .await;
        assert!(resp.get("result").is_some(), "fulfillRequest: {resp}");

        // Drain until navigate completes.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for navigate response"
            );
            let frame = ws_read_frame(&mut ws, &mut ws_left, remaining)
                .await
                .expect("ws closed");
            if frame.get("id") == Some(&json!(3)) {
                assert!(frame.get("result").is_some(), "navigate failed: {frame}");
                break;
            }
        }

        let resp = ws_command(
            &mut ws,
            &mut ws_left,
            5,
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.title + '|' + document.querySelector('#main').textContent",
                "returnByValue": true
            })),
        )
        .await;
        assert_eq!(
            resp["result"]["result"]["value"],
            json!("Fulfilled|intercepted"),
            "fulfilled body not installed: {resp}"
        );
    });
}

#[test]
fn preview_http_and_live_ws() {
    with_cdp(|addr, html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let page_url = format!("http://{html}/");
        let resp = client
            .fetch(&format!("http://{addr}/json/new?url={page_url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap().to_owned();

        let resp = client
            .fetch(&format!("http://{addr}/preview"))
            .await
            .unwrap();
        assert_eq!(resp.status.as_u16(), 200);
        let index = String::from_utf8_lossy(&resp.body);
        assert!(index.contains("Hyperion live preview"), "index: {index}");

        let resp = client
            .fetch(&format!("http://{addr}/preview/{id}"))
            .await
            .unwrap();
        assert_eq!(resp.status.as_u16(), 200);
        let viewer = String::from_utf8_lossy(&resp.body);
        assert!(
            viewer.contains("sandbox"),
            "viewer missing iframe: {viewer}"
        );
        assert!(viewer.contains(&id), "viewer missing target id: {viewer}");

        let (mut preview, mut preview_left) =
            ws_connect(&addr.to_string(), &format!("/preview/live/{id}"))
                .await
                .unwrap();
        let snap = ws_read_frame(&mut preview, &mut preview_left, Duration::from_secs(5))
            .await
            .expect("no initial preview snapshot");
        assert_eq!(snap["type"], "snapshot");
        assert!(
            snap["html"].as_str().unwrap().contains("Hello"),
            "snapshot html: {snap}"
        );

        // Mutate via CDP; preview should receive an updated snapshot.
        let (mut ws, mut ws_left) = ws_connect(&addr.to_string(), &format!("/devtools/page/{id}"))
            .await
            .unwrap();
        ws_command(
            &mut ws,
            &mut ws_left,
            1,
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.querySelector('#main').textContent = 'preview-mut'",
                "returnByValue": true
            })),
        )
        .await;

        let mut saw = false;
        for _ in 0..20 {
            let frame =
                ws_read_frame(&mut preview, &mut preview_left, Duration::from_secs(2)).await;
            let Some(frame) = frame else {
                break;
            };
            if frame["type"] == "snapshot"
                && frame["html"].as_str().unwrap_or("").contains("preview-mut")
            {
                saw = true;
                break;
            }
        }
        assert!(saw, "preview did not reflect JS DOM mutation");
    });
}

#[test]
fn health_live_and_metrics() {
    with_cdp(|cdp_addr, _html| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let live = client
            .fetch(&format!("http://{cdp_addr}/health/live"))
            .await
            .unwrap();
        assert_eq!(live.status.as_u16(), 200);
        let ready = client
            .fetch(&format!("http://{cdp_addr}/health/ready"))
            .await
            .unwrap();
        assert_eq!(ready.status.as_u16(), 200);
        let metrics = client
            .fetch(&format!("http://{cdp_addr}/metrics"))
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&metrics.body);
        assert!(body.contains("hyperion_targets_open"), "{body}");
        let ui = client
            .fetch(&format!("http://{cdp_addr}/ui/"))
            .await
            .unwrap();
        assert_eq!(ui.status.as_u16(), 200);
    });
}

#[test]
fn local_storage_survives_evaluate() {
    with_cdp(|cdp_addr, html_addr| async move {
        let client = HttpClient::new(HttpClientOptions::default());
        let url = format!("http://{html_addr}/");
        let resp = client
            .fetch(&format!("http://{cdp_addr}/json/new?url={url}"))
            .await
            .unwrap();
        let desc: Value = serde_json::from_slice(&resp.body).unwrap();
        let id = desc["id"].as_str().unwrap();
        let (mut ws, mut leftover) =
            ws_connect(&cdp_addr.to_string(), &format!("/devtools/page/{id}"))
                .await
                .unwrap();
        let _ = ws_command(&mut ws, &mut leftover, 1, "Runtime.enable", None).await;
        let r = ws_command(
            &mut ws,
            &mut leftover,
            2,
            "Runtime.evaluate",
            Some(json!({
                "expression": "localStorage.setItem('a','1'); localStorage.getItem('a')",
                "returnByValue": true
            })),
        )
        .await;
        assert_eq!(r["result"]["result"]["value"], "1", "{r}");
    });
}

#[test]
fn bearer_token_protects_control_plane() {
    with_cdp_token(
        Some("test-token-32-bytes-long!!!!!!".into()),
        |cdp_addr, _html| async move {
            let anon = HttpClient::new(HttpClientOptions::default());
            let live = anon
                .fetch(&format!("http://{cdp_addr}/health/live"))
                .await
                .unwrap();
            assert_eq!(live.status.as_u16(), 200);

            let version = anon
                .fetch(&format!("http://{cdp_addr}/json/version"))
                .await
                .unwrap();
            assert_eq!(version.status.as_u16(), 401);

            let ready = anon
                .fetch(&format!("http://{cdp_addr}/health/ready"))
                .await
                .unwrap();
            assert_eq!(ready.status.as_u16(), 401);

            let preview = anon
                .fetch(&format!("http://{cdp_addr}/preview"))
                .await
                .unwrap();
            assert_eq!(preview.status.as_u16(), 401);

            let authed = HttpClient::new(HttpClientOptions::default());
            authed.set_extra_headers(vec![(
                "Authorization".into(),
                "Bearer test-token-32-bytes-long!!!!!!".into(),
            )]);
            let version = authed
                .fetch(&format!("http://{cdp_addr}/json/version"))
                .await
                .unwrap();
            assert_eq!(version.status.as_u16(), 200);

            let wrong = HttpClient::new(HttpClientOptions::default());
            wrong.set_extra_headers(vec![(
                "Authorization".into(),
                "Bearer totally-wrong-token-value!!!!".into(),
            )]);
            let version = wrong
                .fetch(&format!("http://{cdp_addr}/json/version"))
                .await
                .unwrap();
            assert_eq!(version.status.as_u16(), 401);
        },
    );
}

async fn http_exchange(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = glommio::net::TcpStream::connect(addr.to_string())
        .await
        .unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_owned(), rest.to_owned())
}

#[test]
fn target_limit_overloads_and_close_updates_metrics() {
    with_cdp_config(
        |c| c.max_targets = 1,
        |cdp_addr, html_addr| async move {
            let client = HttpClient::new(HttpClientOptions::default());
            let url = format!("http://{html_addr}/");
            let first = client
                .fetch(&format!("http://{cdp_addr}/json/new?url={url}"))
                .await
                .unwrap();
            let desc: Value = serde_json::from_slice(&first.body).unwrap();
            let id = desc["id"].as_str().expect("first target id").to_owned();

            let second = client
                .fetch(&format!("http://{cdp_addr}/json/new?url={url}"))
                .await
                .unwrap();
            let err: Value = serde_json::from_slice(&second.body).unwrap();
            assert!(
                err["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Overloaded:"),
                "{err}"
            );

            let metrics = client
                .fetch(&format!("http://{cdp_addr}/metrics"))
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&metrics.body);
            assert!(text.contains("hyperion_admission_rejected_total"), "{text}");

            let _ = client
                .fetch(&format!("http://{cdp_addr}/json/close/{id}"))
                .await
                .unwrap();
            let metrics = client
                .fetch(&format!("http://{cdp_addr}/metrics"))
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&metrics.body);
            assert!(
                text.contains("hyperion_targets_open 1\n")
                    || text.contains("hyperion_targets_open 1\r"),
                "expected browser-only target after close:\n{text}"
            );
        },
    );
}

#[test]
fn eval_admission_rejects_when_limit_is_zero() {
    with_cdp_config(
        |c| c.max_active_evals = 0,
        |cdp_addr, html_addr| async move {
            let client = HttpClient::new(HttpClientOptions::default());
            let url = format!("http://{html_addr}/");
            let resp = client
                .fetch(&format!("http://{cdp_addr}/json/new?url={url}"))
                .await
                .unwrap();
            let desc: Value = serde_json::from_slice(&resp.body).unwrap();
            let id = desc["id"].as_str().unwrap();
            let (mut ws, mut leftover) =
                ws_connect(&cdp_addr.to_string(), &format!("/devtools/page/{id}"))
                    .await
                    .unwrap();
            let r = ws_command(
                &mut ws,
                &mut leftover,
                1,
                "Runtime.evaluate",
                Some(json!({ "expression": "1+1", "returnByValue": true })),
            )
            .await;
            assert!(
                r["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Overloaded:"),
                "{r}"
            );
        },
    );
}

#[test]
fn dashboard_catalog_and_csrf() {
    with_cdp_config(
        |c| {
            c.auth_token = Some("dashboard-secret-token-value!!".into());
        },
        |cdp_addr, _html| async move {
            let (status, head, body) = http_exchange(
                cdp_addr,
                "POST",
                "/api/v1/dashboard/login",
                &[("Authorization", "Bearer dashboard-secret-token-value!!")],
                "{}",
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let json: Value = serde_json::from_str(&body).unwrap();
            let csrf = json["csrf"].as_str().unwrap().to_owned();
            let cookie = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().split(';').next().unwrap().to_owned())
                .expect("session cookie");

            let (status, _, body) = http_exchange(
                cdp_addr,
                "POST",
                "/api/v1/dashboard/profiles?action=create&name=ops",
                &[("Cookie", &cookie)],
                "",
            )
            .await;
            assert_eq!(status, 403, "mutating dashboard calls require CSRF: {body}");

            let (status, _, body) = http_exchange(
                cdp_addr,
                "POST",
                "/api/v1/dashboard/profiles?action=create&name=ops",
                &[("Cookie", &cookie), ("x-csrf-token", &csrf)],
                "",
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let created: Value = serde_json::from_str(&body).unwrap();
            assert!(created["profileId"].as_str().is_some(), "{created}");

            let (status, _, body) = http_exchange(
                cdp_addr,
                "GET",
                "/api/v1/dashboard/profiles?offset=0&limit=50",
                &[("Cookie", &cookie)],
                "",
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let listed: Value = serde_json::from_str(&body).unwrap();
            let profiles = listed["profiles"].as_array().cloned().unwrap_or_default();
            assert!(
                profiles.iter().any(|p| p["name"] == "ops"),
                "catalog listing missing created profile: {listed}"
            );

            let (status, _, body) = http_exchange(
                cdp_addr,
                "GET",
                "/api/v1/dashboard/nodes",
                &[("Cookie", &cookie)],
                "",
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let nodes: Value = serde_json::from_str(&body).unwrap();
            assert!(nodes["workers"].as_array().is_some(), "{nodes}");
        },
    );
}

#[test]
fn target_create_uses_and_disposes_requested_browser_context() {
    with_cdp(|cdp_addr, _html_addr| async move {
        let (mut ws, mut leftover) = ws_connect(&cdp_addr.to_string(), "/devtools/browser/browser")
            .await
            .unwrap();

        let context = ws_command(
            &mut ws,
            &mut leftover,
            1,
            "Target.createBrowserContext",
            Some(json!({})),
        )
        .await;
        let context_id = context["result"]["browserContextId"]
            .as_str()
            .expect("browser context id")
            .to_owned();

        let created = ws_command(
            &mut ws,
            &mut leftover,
            2,
            "Target.createTarget",
            Some(json!({
                "url": "about:blank",
                "browserContextId": context_id,
            })),
        )
        .await;
        let target_id = created["result"]["targetId"]
            .as_str()
            .expect("target id")
            .to_owned();

        let info = ws_command(
            &mut ws,
            &mut leftover,
            3,
            "Target.getTargetInfo",
            Some(json!({ "targetId": target_id })),
        )
        .await;
        assert_eq!(
            info["result"]["targetInfo"]["browserContextId"], context_id,
            "{info}"
        );

        let invalid = ws_command(
            &mut ws,
            &mut leftover,
            4,
            "Target.createTarget",
            Some(json!({
                "url": "about:blank",
                "browserContextId": "missing-context",
            })),
        )
        .await;
        assert!(
            invalid["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("browser context"),
            "{invalid}"
        );

        let disposed = ws_command(
            &mut ws,
            &mut leftover,
            5,
            "Target.disposeBrowserContext",
            Some(json!({ "browserContextId": context_id })),
        )
        .await;
        assert!(disposed.get("error").is_none(), "{disposed}");

        let targets = ws_command(
            &mut ws,
            &mut leftover,
            6,
            "Target.getTargets",
            Some(json!({})),
        )
        .await;
        let target_infos = targets["result"]["targetInfos"]
            .as_array()
            .expect("target infos");
        assert!(
            target_infos.iter().all(|t| t["targetId"] != target_id),
            "{targets}"
        );

        let contexts = ws_command(
            &mut ws,
            &mut leftover,
            7,
            "Target.getBrowserContexts",
            Some(json!({})),
        )
        .await;
        let context_ids = contexts["result"]["browserContextIds"]
            .as_array()
            .expect("browser context ids");
        assert!(context_ids.iter().all(|id| id != &context_id), "{contexts}");
    });
}
