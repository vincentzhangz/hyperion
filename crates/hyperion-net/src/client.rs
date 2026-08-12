//! HTTP client driving hyper over glommio sockets.
//!
//! Idle HTTP/1 senders are pooled per origin. Connections the peer closed
//! (`Connection: close` or a failed `ready()`) are discarded and replaced.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use glommio::timer::Timer;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use rustls::pki_types::ServerName;
use url::Url;

use crate::connector::{GlommioStream, TlsStream};
use crate::cookie::CookieJar;
use crate::error::{Error, Result};
use crate::ssrf;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_USER_AGENT: &str = "hyperion/0.1";

/// A network event observed while fetching. Hooked up by the CDP layer to
/// synthesize `Network.*` protocol events.
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// The request is about to be sent.
    Request {
        id: u64,
        url: String,
        method: String,
        headers: http::HeaderMap,
        target_id: Option<String>,
    },
    /// Response headers arrived for a request.
    Response {
        id: u64,
        url: String,
        status: u16,
        headers: http::HeaderMap,
        target_id: Option<String>,
    },
    /// Response body fully collected.
    Finished {
        id: u64,
        body: Bytes,
        target_id: Option<String>,
    },
    /// The request failed (connect, TLS, timeout or transfer error).
    Failed {
        id: u64,
        url: String,
        error: String,
        target_id: Option<String>,
    },
}

/// A sink for [`NetEvent`]s. Cheap to call; the CDP layer forwards them to the
/// relevant sessions.
pub type NetEventSink = Arc<dyn Fn(NetEvent) + Send + Sync>;

/// Decision for a paused fetch (CDP `Fetch.*`).
#[derive(Debug, Clone)]
pub enum FetchAction {
    Continue {
        url: Option<String>,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
    Fulfill {
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
    },
    Fail {
        error_reason: String,
    },
}

/// Shared state for CDP Fetch interception. Lives on the executor thread.
#[derive(Clone, Default)]
pub struct FetchIntercept {
    inner: Rc<RefCell<FetchInterceptInner>>,
}

#[derive(Default)]
struct FetchInterceptInner {
    enabled: bool,
    /// Patterns; empty means match all.
    patterns: Vec<String>,
    decisions: HashMap<String, FetchAction>,
}

impl FetchIntercept {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&self, enabled: bool, patterns: Vec<String>) {
        let mut inner = self.inner.borrow_mut();
        inner.enabled = enabled;
        inner.patterns = patterns;
        if !enabled {
            inner.decisions.clear();
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.borrow().enabled
    }

    pub fn provide_decision(&self, request_id: &str, action: FetchAction) {
        self.inner
            .borrow_mut()
            .decisions
            .insert(request_id.to_owned(), action);
    }

    fn take_decision(&self, request_id: &str) -> Option<FetchAction> {
        self.inner.borrow_mut().decisions.remove(request_id)
    }

    fn matches(&self, url: &str) -> bool {
        let inner = self.inner.borrow();
        if !inner.enabled {
            return false;
        }
        if inner.patterns.is_empty() {
            return true;
        }
        inner.patterns.iter().any(|p| {
            if let Some(prefix) = p.strip_suffix('*') {
                url.starts_with(prefix)
            } else {
                url == p || url.contains(p)
            }
        })
    }
}

/// Called when a request is paused for Fetch interception. Must eventually
/// cause a [`FetchIntercept::provide_decision`] for the same request id.
///
/// Not `Send`/`Sync`: Hyperion's HTTP client and CDP emitter live on the
/// single Glommio executor thread and may capture `Rc` session sinks.
pub type FetchPausedSink = Rc<dyn Fn(u64, String, String, http::HeaderMap)>;

#[derive(Clone)]
pub struct HttpClientOptions {
    pub timeout: Duration,
    pub user_agent: String,
    pub max_redirects: u32,
    pub net_events: Option<NetEventSink>,
    pub extra_headers: Vec<(String, String)>,
    pub fetch_intercept: Option<FetchIntercept>,
    pub fetch_paused: Option<FetchPausedSink>,
    pub allow_private_network: bool,
    pub max_body_bytes: usize,
    pub fetch_pause_timeout: Duration,
    pub target_id: Option<String>,
    pub per_origin_connections: usize,
}

impl std::fmt::Debug for HttpClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClientOptions")
            .field("timeout", &self.timeout)
            .field("user_agent", &self.user_agent)
            .field("max_redirects", &self.max_redirects)
            .field("net_events", &self.net_events.is_some())
            .field("extra_headers", &self.extra_headers.len())
            .field("fetch_intercept", &self.fetch_intercept.is_some())
            .finish()
    }
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            max_redirects: 5,
            net_events: None,
            extra_headers: Vec::new(),
            fetch_intercept: None,
            fetch_paused: None,
            allow_private_network: true,
            max_body_bytes: 16 * 1024 * 1024,
            fetch_pause_timeout: Duration::from_secs(30),
            target_id: None,
            per_origin_connections: 8,
        }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub url: String,
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

pub struct HttpClient {
    options: RefCell<HttpClientOptions>,
    cookies: RefCell<CookieJar>,
    next_request_id: AtomicU64,
    /// Idle HTTP/1 senders keyed by `scheme://host:port`.
    pool: RefCell<HashMap<String, Vec<http1::SendRequest<Full<Bytes>>>>>,
    origin_inflight: RefCell<HashMap<String, usize>>,
}

impl HttpClient {
    pub fn new(options: HttpClientOptions) -> Self {
        Self {
            options: RefCell::new(options),
            cookies: RefCell::new(CookieJar::new()),
            next_request_id: AtomicU64::new(1),
            pool: RefCell::new(HashMap::new()),
            origin_inflight: RefCell::new(HashMap::new()),
        }
    }

    pub fn set_target_id(&self, id: Option<String>) {
        self.options.borrow_mut().target_id = id;
    }

    pub fn set_cookies(&self, jar: CookieJar) {
        *self.cookies.borrow_mut() = jar;
    }

    pub fn origin_key(parsed: &Url) -> String {
        let host = parsed.host_str().unwrap_or("localhost");
        let port = parsed.port_or_known_default().unwrap_or(80);
        format!("{}://{host}:{port}", parsed.scheme())
    }

    pub fn with_options(options: HttpClientOptions) -> Self {
        Self::new(options)
    }

    pub fn options(&self) -> std::cell::Ref<'_, HttpClientOptions> {
        self.options.borrow()
    }

    pub fn set_user_agent(&self, ua: &str) {
        self.options.borrow_mut().user_agent = ua.to_owned();
    }

    pub fn set_extra_headers(&self, headers: Vec<(String, String)>) {
        self.options.borrow_mut().extra_headers = headers;
    }

    pub fn cookies(&self) -> std::cell::Ref<'_, CookieJar> {
        self.cookies.borrow()
    }

    pub fn cookies_mut(&self) -> std::cell::RefMut<'_, CookieJar> {
        self.cookies.borrow_mut()
    }

    pub fn fetch_intercept(&self) -> Option<FetchIntercept> {
        self.options.borrow().fetch_intercept.clone()
    }

    /// Fetch a URL, following redirects up to `max_redirects`.
    pub async fn fetch(&self, url: &str) -> Result<HttpResponse> {
        let (max_redirects, allow_private) = {
            let o = self.options.borrow();
            (o.max_redirects, o.allow_private_network)
        };
        let mut current = url.to_owned();
        for _ in 0..=max_redirects {
            ssrf::assert_url_allowed(&current, allow_private)?;
            let resp = self.fetch_once(&current).await?;
            if let Some(location) = resp
                .headers
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            {
                let next = Url::parse(&current)?.join(location)?;
                if next.scheme() == "http" || next.scheme() == "https" {
                    ssrf::assert_parsed_allowed(&next, allow_private)?;
                    current = next.to_string();
                    continue;
                }
            }
            return Ok(resp);
        }
        Err(Error::Io(std::io::Error::other("too many redirects")))
    }

    async fn fetch_once(&self, url: &str) -> Result<HttpResponse> {
        let parsed = Url::parse(url)?;
        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(Error::Scheme(scheme.to_owned()));
        }
        let host = parsed.host_str().ok_or(Error::NoHost)?.to_owned();
        let port = parsed.port_or_known_default().unwrap_or(80);
        let timeout = self.options.borrow().timeout;
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let cookie_header = {
            let cookies = self.cookies.borrow().get(&parsed);
            cookies
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        };

        // Fetch interception: pause before the network round-trip.
        let intercept = self.options.borrow().fetch_intercept.clone();
        if let Some(intercept) = intercept
            && intercept.matches(url)
        {
            let method = "GET".to_owned();
            let headers = http::HeaderMap::new();
            let paused = self.options.borrow().fetch_paused.clone();
            if let Some(sink) = paused {
                hyperion_metrics::global()
                    .fetch_pauses
                    .fetch_add(1, Ordering::Relaxed);
                sink(request_id, url.to_owned(), method.clone(), headers.clone());
            }
            let action = self.wait_fetch_decision(&intercept, request_id).await;
            match action {
                FetchAction::Fail { error_reason } => {
                    self.emit_failed(request_id, url, &error_reason);
                    return Err(Error::Io(std::io::Error::other(error_reason)));
                }
                FetchAction::Fulfill {
                    status,
                    headers,
                    body,
                } => {
                    let mut map = http::HeaderMap::new();
                    for (k, v) in headers {
                        if let (Ok(name), Ok(val)) = (
                            http::HeaderName::try_from(k),
                            http::HeaderValue::try_from(v),
                        ) {
                            map.insert(name, val);
                        }
                    }
                    if let Some(sink) = &self.options.borrow().net_events {
                        let tid = self.options.borrow().target_id.clone();
                        sink(NetEvent::Request {
                            id: request_id,
                            url: url.to_owned(),
                            method,
                            headers: map.clone(),
                            target_id: tid.clone(),
                        });
                        sink(NetEvent::Response {
                            id: request_id,
                            url: url.to_owned(),
                            status,
                            headers: map.clone(),
                            target_id: tid.clone(),
                        });
                        sink(NetEvent::Finished {
                            id: request_id,
                            body: body.clone(),
                            target_id: tid,
                        });
                    }
                    return Ok(HttpResponse {
                        url: url.to_owned(),
                        status: http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK),
                        headers: map,
                        body,
                    });
                }
                FetchAction::Continue {
                    url: new_url,
                    method: _,
                    headers: _,
                } => {
                    if let Some(u) = new_url {
                        return Box::pin(self.fetch_once(&u)).await;
                    }
                    // Fall through to normal fetch.
                }
            }
        }

        let result = futures_lite::future::race(
            self.fetch_transport(request_id, &parsed, &host, port, &cookie_header),
            async {
                Timer::new(timeout).await;
                self.emit_failed(request_id, url, "request timed out");
                Err(Error::Timeout)
            },
        )
        .await?;

        let set_cookies: Vec<String> = result
            .headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or_default().to_owned())
            .collect();
        if !set_cookies.is_empty() {
            let mut jar = self.cookies.borrow_mut();
            for sc in set_cookies {
                jar.set(&parsed, &sc);
            }
        }
        Ok(result)
    }

    /// Poll until CDP provides a Fetch decision for `request_id`.
    async fn wait_fetch_decision(
        &self,
        intercept: &FetchIntercept,
        request_id: u64,
    ) -> FetchAction {
        let key = request_id.to_string();
        let deadline = std::time::Instant::now() + self.options.borrow().fetch_pause_timeout;
        loop {
            if let Some(action) = intercept.take_decision(&key) {
                return action;
            }
            if std::time::Instant::now() >= deadline {
                hyperion_metrics::global()
                    .fetch_timeouts
                    .fetch_add(1, Ordering::Relaxed);
                return FetchAction::Fail {
                    error_reason: "Fetch.paused timeout".into(),
                };
            }
            Timer::new(Duration::from_millis(5)).await;
        }
    }

    /// Connect (optionally over TLS, per scheme) and perform the request.
    async fn fetch_transport(
        &self,
        request_id: u64,
        parsed: &Url,
        host: &str,
        port: u16,
        cookie_header: &str,
    ) -> Result<HttpResponse> {
        let origin = Self::origin_key(parsed);
        let limit = self.options.borrow().per_origin_connections.max(1);
        loop {
            let n = self
                .origin_inflight
                .borrow()
                .get(&origin)
                .copied()
                .unwrap_or(0);
            if n < limit {
                break;
            }
            Timer::new(Duration::from_millis(5)).await;
        }
        {
            let mut inf = self.origin_inflight.borrow_mut();
            *inf.entry(origin.clone()).or_insert(0) += 1;
        }

        let pooled = self
            .pool
            .borrow_mut()
            .get_mut(&origin)
            .and_then(|v| v.pop());
        let result = if let Some(mut sender) = pooled {
            match sender.ready().await {
                Ok(()) => {
                    hyperion_metrics::global()
                        .http_pool_reuse
                        .fetch_add(1, Ordering::Relaxed);
                    self.request_with_sender(
                        request_id,
                        parsed,
                        host,
                        cookie_header,
                        sender,
                        &origin,
                    )
                    .await
                }
                Err(_) => {
                    self.connect_and_request(request_id, parsed, host, port, cookie_header, &origin)
                        .await
                }
            }
        } else {
            self.connect_and_request(request_id, parsed, host, port, cookie_header, &origin)
                .await
        };

        {
            let mut inf = self.origin_inflight.borrow_mut();
            if let Some(n) = inf.get_mut(&origin) {
                *n = n.saturating_sub(1);
            }
        }
        result
    }

    async fn connect_and_request(
        &self,
        request_id: u64,
        parsed: &Url,
        host: &str,
        port: u16,
        cookie_header: &str,
        origin: &str,
    ) -> Result<HttpResponse> {
        let url = parsed.to_string();
        let addr = format!("{host}:{port}");
        let stream = match glommio::net::TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                self.emit_failed(request_id, &url, &e.to_string());
                return Err(Error::Io(e.into()));
            }
        };

        if parsed.scheme() == "https" {
            let server_name =
                ServerName::try_from(host.to_owned()).map_err(|e| Error::Tls(e.to_string()))?;
            let tls = futures_rustls::TlsConnector::from(tls_config());
            let stream = match tls.connect(server_name, GlommioStream::new(stream)).await {
                Ok(s) => s,
                Err(e) => {
                    self.emit_failed(request_id, &url, &e.to_string());
                    return Err(Error::Tls(e.to_string()));
                }
            };
            self.handshake_and_request(
                request_id,
                parsed,
                host,
                TlsStream::new(stream),
                cookie_header,
                origin,
            )
            .await
        } else {
            self.handshake_and_request(
                request_id,
                parsed,
                host,
                GlommioStream::new(stream),
                cookie_header,
                origin,
            )
            .await
        }
    }

    async fn handshake_and_request<IO>(
        &self,
        request_id: u64,
        parsed: &Url,
        host: &str,
        io: IO,
        cookie_header: &str,
        origin: &str,
    ) -> Result<HttpResponse>
    where
        IO: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let url = parsed.to_string();
        let (sender, conn) = match http1::Builder::new().handshake(io).await {
            Ok(h) => h,
            Err(e) => {
                self.emit_failed(request_id, &url, &e.to_string());
                return Err(Error::Hyper(e));
            }
        };
        glommio::spawn_local(conn).detach();
        self.request_with_sender(request_id, parsed, host, cookie_header, sender, origin)
            .await
    }

    async fn request_with_sender(
        &self,
        request_id: u64,
        parsed: &Url,
        host: &str,
        cookie_header: &str,
        mut sender: http1::SendRequest<Full<Bytes>>,
        origin: &str,
    ) -> Result<HttpResponse> {
        let url = parsed.to_string();
        let (user_agent, extra_headers) = {
            let opts = self.options.borrow();
            (opts.user_agent.clone(), opts.extra_headers.clone())
        };

        let mut builder = http::Request::builder()
            .method(http::Method::GET)
            .uri(uri_for(parsed))
            .header(http::header::HOST, host)
            .header(http::header::USER_AGENT, user_agent)
            .header(http::header::CONNECTION, "keep-alive");
        if !cookie_header.is_empty() {
            builder = builder.header(http::header::COOKIE, cookie_header);
        }
        for (k, v) in &extra_headers {
            if let Ok(name) = http::HeaderName::try_from(k.as_str()) {
                builder = builder.header(name, v.as_str());
            }
        }
        if let Err(e) = sender.ready().await {
            self.emit_failed(request_id, &url, &e.to_string());
            return Err(Error::Hyper(e));
        }
        let req = builder.body(Full::new(Bytes::new())).map_err(Error::Http)?;
        if let Some(sink) = &self.options.borrow().net_events {
            sink(NetEvent::Request {
                id: request_id,
                url: url.clone(),
                method: req.method().as_str().to_owned(),
                headers: req.headers().clone(),
                target_id: self.options.borrow().target_id.clone(),
            });
        }

        let resp = match sender.send_request(req).await {
            Ok(r) => r,
            Err(e) => {
                self.emit_failed(request_id, &url, &e.to_string());
                return Err(Error::Hyper(e));
            }
        };

        let status = resp.status();
        let headers = resp.headers().clone();
        if let Some(sink) = &self.options.borrow().net_events {
            sink(NetEvent::Response {
                id: request_id,
                url: url.clone(),
                status: status.as_u16(),
                headers: headers.clone(),
                target_id: self.options.borrow().target_id.clone(),
            });
        }

        let max_body = self.options.borrow().max_body_bytes;
        let body = match resp.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                self.emit_failed(request_id, &url, &e.to_string());
                return Err(Error::Hyper(e));
            }
        };
        if body.len() > max_body {
            self.emit_failed(request_id, &url, "response body exceeds limit");
            return Err(Error::BodyTooLarge);
        }
        if let Some(sink) = &self.options.borrow().net_events {
            sink(NetEvent::Finished {
                id: request_id,
                body: body.clone(),
                target_id: self.options.borrow().target_id.clone(),
            });
        }
        if should_keep_alive(&headers) && !sender.is_closed() {
            let cap = self.options.borrow().per_origin_connections.max(1);
            let mut pool = self.pool.borrow_mut();
            let idle = pool.entry(origin.to_owned()).or_default();
            if idle.len() < cap {
                idle.push(sender);
            }
        }
        Ok(HttpResponse {
            url,
            status,
            headers,
            body,
        })
    }

    /// Emit `NetEvent::Failed` for `err`, if a sink is installed.
    fn emit_failed(&self, id: u64, url: &str, error: &str) {
        if let Some(sink) = &self.options.borrow().net_events {
            sink(NetEvent::Failed {
                id,
                url: url.to_owned(),
                error: error.to_owned(),
                target_id: self.options.borrow().target_id.clone(),
            });
        }
    }
}

/// A TLS client config trusted against the Mozilla root store. Built once and
/// shared by all requests.
fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    static TLS_CONFIG: OnceLock<std::sync::Arc<rustls::ClientConfig>> = OnceLock::new();
    TLS_CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            std::sync::Arc::new(config)
        })
        .clone()
}

fn uri_for(url: &Url) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    match url.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    }
}

fn should_keep_alive(headers: &http::HeaderMap) -> bool {
    match headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => true, // HTTP/1.1 default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_config_loads_mozilla_roots() {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        assert!(
            roots.len() > 100,
            "expected Mozilla roots to be loaded, got {}",
            roots.len()
        );
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let _ = config;
    }

    #[test]
    fn server_name_accepts_dns_and_ip() {
        assert!(ServerName::try_from("example.com".to_owned()).is_ok());
        assert!(ServerName::try_from("93.184.216.34".to_owned()).is_ok());
        assert!(ServerName::try_from("bad host name".to_owned()).is_err());
    }

    #[test]
    fn uri_for_preserves_query() {
        let url = Url::parse("https://example.com/path?q=1&b=2").unwrap();
        assert_eq!(uri_for(&url), "/path?q=1&b=2");
        let root = Url::parse("https://example.com").unwrap();
        assert_eq!(uri_for(&root), "/");
    }
}
