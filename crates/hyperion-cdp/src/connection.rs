//! Per-connection HTTP + WebSocket handling.
//!
//! Each accepted socket becomes a [`ConnectionTask`]: it reads the HTTP
//! request head, serves the `/json/*` endpoints directly, or upgrades to a
//! WebSocket (`/devtools/page/<id>`, `/devtools/browser/<id>`) where a reader
//! loop and a writer task talk to the browser task over glommio `local_channel`s.

use std::io;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::channels::local_channel::{LocalReceiver, LocalSender};
use serde_json::Value;

use crate::preview::PreviewClientMsg;
use crate::protocol::{self, ErrorObj};
use crate::ws::{self, Frame, SharedStream, WsReader, WsWriter};
use bytes::Bytes;
use hyperion_config::Config;
use hyperion_dashboard::DashboardState;
use hyperion_net::client::{FetchAction, FetchIntercept};

/// A shareable, single-threaded channel endpoint.
pub type Sender<T> = Rc<LocalSender<T>>;
/// A shareable, single-threaded channel endpoint.
pub type Receiver<T> = Rc<LocalReceiver<T>>;
/// One command reply: either the result value or a protocol error object.
pub type Reply = Result<Value, ErrorObj>;

/// Messages the writer task turns into frames on the socket.
#[derive(Debug)]
pub enum OutMsg {
    Text(String),
    Pong(Vec<u8>),
    Close,
}

/// Requests the browser task can execute. Every variant carries the reply
/// channel; the browser task sends `Ok(Value)` or `Err(ErrorObj)` back.
#[derive(Debug)]
pub enum CdpRequest {
    Version {
        reply: Sender<Reply>,
    },
    ListTargets {
        reply: Sender<Reply>,
    },
    NewTarget {
        url: String,
        reply: Sender<Reply>,
    },
    CloseTarget {
        id: String,
        reply: Sender<Reply>,
    },
    Activate {
        id: String,
        reply: Sender<Reply>,
    },
    RegisterSession {
        target_id: String,
        out: Sender<OutMsg>,
        reply: Sender<Reply>,
    },
    InvokeCommand {
        session_id: String,
        target_id: String,
        request: protocol::Request,
        reply: Sender<Reply>,
    },
    DetachSession {
        session_id: String,
        reply: Sender<Reply>,
    },
    /// Live-preview WebSocket: register for snapshot + delta feed.
    RegisterPreview {
        target_id: String,
        out: Sender<OutMsg>,
        reply: Sender<Reply>,
    },
    /// Push a fresh HTML snapshot to preview subscribers for `target_id`.
    PushPreviewSnapshot {
        target_id: String,
        reply: Sender<Reply>,
    },
    DetachPreview {
        target_id: String,
        reply: Sender<Reply>,
    },
    /// Operator preview interaction (click/hover/key/scroll).
    PreviewCommand {
        target_id: String,
        profile_id: Option<String>,
        msg: PreviewClientMsg,
        reply: Sender<Reply>,
    },
    Shutdown {
        reply: Sender<Reply>,
    },
    Dashboard {
        path: String,
        query: Option<String>,
        method: String,
        reply: Sender<Reply>,
    },
}

/// A parsed HTTP request head.
struct HttpRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
}

enum Route {
    Json(RequestKind),
    Ws { target_id: String },
    PreviewIndex,
    PreviewPage { target_id: String },
    PreviewLive { target_id: String },
    Live,
    Ready,
    Metrics,
    Ui,
    UiAsset { name: String },
    DashboardApi { path: String },
    NotFound,
}

enum RequestKind {
    Version,
    List,
    New(String),
    Close(String),
    Activate(String),
}

pub struct ConnectionTask {
    stream: glommio::net::TcpStream,
    cmd: Sender<CdpRequest>,
    fetch: Option<FetchIntercept>,
    config: Config,
    dashboard: DashboardState,
}

impl ConnectionTask {
    pub fn new(
        stream: glommio::net::TcpStream,
        cmd: Sender<CdpRequest>,
        fetch: Option<FetchIntercept>,
        config: Config,
        dashboard: DashboardState,
    ) -> Self {
        Self {
            stream,
            cmd,
            fetch,
            config,
            dashboard,
        }
    }

    pub async fn run(mut self) -> io::Result<()> {
        let (reply_tx, reply_rx) = new_reply_channel();
        let max_header = self.config.max_header_bytes;
        let (req, leftover) = match read_http_head(&mut self.stream, max_header).await {
            Ok(v) => v,
            Err(e) => {
                let _ = write_http(&mut self.stream, 400, "text/plain", "bad request").await;
                return Err(e);
            }
        };
        let route = classify(&req);
        if !matches!(route, Route::Live | Route::Ui | Route::UiAsset { .. })
            && !authorized(&self.config, &self.dashboard, &req)
        {
            return write_http(&mut self.stream, 401, "text/plain", "unauthorized").await;
        }
        match route {
            Route::Live => write_http(&mut self.stream, 200, "text/plain", "ok").await,
            Route::Ready => {
                let shutting = hyperion_metrics::global()
                    .shutdown
                    .load(std::sync::atomic::Ordering::Relaxed)
                    != 0;
                if shutting {
                    write_http(&mut self.stream, 503, "text/plain", "draining").await
                } else {
                    write_http(&mut self.stream, 200, "text/plain", "ready").await
                }
            }
            Route::Metrics => {
                write_http(
                    &mut self.stream,
                    200,
                    "text/plain; version=0.0.4",
                    &hyperion_metrics::global().render_prometheus(),
                )
                .await
            }
            Route::Ui => {
                write_http(
                    &mut self.stream,
                    200,
                    "text/html; charset=UTF-8",
                    self.dashboard.index_html(),
                )
                .await
            }
            Route::UiAsset { name } => {
                let (ct, body) = match name.as_str() {
                    "app.js" => (
                        "application/javascript; charset=UTF-8",
                        self.dashboard.app_js(),
                    ),
                    "app.css" => ("text/css; charset=UTF-8", self.dashboard.app_css()),
                    _ => return write_http(&mut self.stream, 404, "text/plain", "not found").await,
                };
                write_http(&mut self.stream, 200, ct, body).await
            }
            Route::DashboardApi { path } => self.dashboard_api(&req, &path).await,
            Route::Json(request) => {
                let result = self.roundtrip(request, &reply_tx, &reply_rx).await;
                let body = match &result {
                    Ok(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()),
                    Err(e) => serde_json::to_string(e).unwrap_or_else(|_| "{}".to_owned()),
                };
                write_http(
                    &mut self.stream,
                    200,
                    "application/json; charset=UTF-8",
                    &body,
                )
                .await
            }
            Route::Ws { target_id } => {
                let Some(key) = req
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, v)| v.clone())
                else {
                    return Ok(());
                };
                let accept = ws::accept_key(&key);
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                write_all(&mut self.stream, resp.as_bytes()).await?;
                self.ws_loop(target_id, leftover, &reply_tx, &reply_rx)
                    .await
            }
            Route::PreviewLive { target_id } => {
                if !origin_allowed(&req) {
                    return write_http(&mut self.stream, 403, "text/plain", "origin").await;
                }
                let Some(key) = req
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, v)| v.clone())
                else {
                    return Ok(());
                };
                let session_sid =
                    header_value(&req, "cookie").and_then(hyperion_dashboard::session_from_cookie);
                let csrf_required = self.config.auth_required()
                    && bearer_token(&req).is_none()
                    && session_sid.is_some();
                let accept = ws::accept_key(&key);
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                write_all(&mut self.stream, resp.as_bytes()).await?;
                self.preview_ws_loop(
                    target_id,
                    leftover,
                    &reply_tx,
                    &reply_rx,
                    csrf_required,
                    session_sid,
                )
                .await
            }
            Route::PreviewIndex => {
                write_http(
                    &mut self.stream,
                    200,
                    "text/html; charset=UTF-8",
                    crate::preview::INDEX_HTML,
                )
                .await
            }
            Route::PreviewPage { target_id } => {
                write_http(
                    &mut self.stream,
                    200,
                    "text/html; charset=UTF-8",
                    &crate::preview::viewer_html(&target_id),
                )
                .await
            }
            Route::NotFound => write_http(&mut self.stream, 404, "text/plain", "not found").await,
        }
    }

    async fn dashboard_api(&mut self, req: &HttpRequest, path: &str) -> io::Result<()> {
        if path == "/api/v1/dashboard/login" {
            let presented = bearer_token(req).unwrap_or("");
            match self.dashboard.login(presented) {
                Some((sid, csrf)) => {
                    let forwarded_https = header_value(req, "x-forwarded-proto")
                        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
                    let cookie = hyperion_dashboard::login_set_cookie(
                        &sid,
                        self.config.session_ttl,
                        forwarded_https,
                    );
                    let body = serde_json::json!({ "csrf": csrf }).to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    write_all(&mut self.stream, resp.as_bytes()).await
                }
                None => write_http(&mut self.stream, 401, "text/plain", "unauthorized").await,
            }
        } else {
            let method = req.method.to_ascii_uppercase();
            if method == "POST" && path != "/api/v1/dashboard/login" {
                let csrf = header_value(req, "x-csrf-token").unwrap_or("");
                let cookie = header_value(req, "cookie").unwrap_or("");
                let sid = hyperion_dashboard::session_from_cookie(cookie).unwrap_or_default();
                if !self.dashboard.csrf_ok(&sid, csrf) {
                    return write_http(&mut self.stream, 403, "text/plain", "csrf").await;
                }
            } else if method != "GET" && method != "HEAD" && method != "POST" {
                return write_http(&mut self.stream, 405, "text/plain", "method not allowed").await;
            }
            let (reply_tx, reply_rx) = new_reply_channel();
            if let Err(e) = enqueue(
                &self.cmd,
                &self.config,
                CdpRequest::Dashboard {
                    path: path.to_owned(),
                    query: req.query.clone(),
                    method,
                    reply: reply_tx,
                },
            ) {
                let body = serde_json::to_string(&e).unwrap_or_else(|_| "{}".into());
                return write_http(
                    &mut self.stream,
                    200,
                    "application/json; charset=UTF-8",
                    &body,
                )
                .await;
            }
            let body = match reply_rx.recv().await {
                Some(Ok(v)) => serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
                Some(Err(e)) => serde_json::to_string(&e).unwrap_or_else(|_| "{}".into()),
                None => "{}".into(),
            };
            write_http(
                &mut self.stream,
                200,
                "application/json; charset=UTF-8",
                &body,
            )
            .await
        }
    }

    async fn roundtrip(
        &self,
        request: RequestKind,
        reply_tx: &Sender<Reply>,
        reply_rx: &Receiver<Reply>,
    ) -> Reply {
        let request = match request {
            RequestKind::Version => CdpRequest::Version {
                reply: reply_tx.clone(),
            },
            RequestKind::List => CdpRequest::ListTargets {
                reply: reply_tx.clone(),
            },
            RequestKind::New(url) => CdpRequest::NewTarget {
                url,
                reply: reply_tx.clone(),
            },
            RequestKind::Close(id) => CdpRequest::CloseTarget {
                id,
                reply: reply_tx.clone(),
            },
            RequestKind::Activate(id) => CdpRequest::Activate {
                id,
                reply: reply_tx.clone(),
            },
        };
        enqueue(&self.cmd, &self.config, request)?;
        reply_rx
            .recv()
            .await
            .ok_or_else(|| ErrorObj::server("server closed the reply channel"))?
    }

    /// WebSocket loop: register a session, spawn the writer task, then drive
    /// the reader, forwarding commands to the browser task.
    async fn ws_loop(
        self,
        target_id: String,
        leftover: Vec<u8>,
        reply_tx: &Sender<Reply>,
        reply_rx: &Receiver<Reply>,
    ) -> io::Result<()> {
        let (out_tx, out_rx) = new_out_channel();
        enqueue(
            &self.cmd,
            &self.config,
            CdpRequest::RegisterSession {
                target_id: target_id.clone(),
                out: out_tx.clone(),
                reply: reply_tx.clone(),
            },
        )
        .map_err(|e| io::Error::other(e.message))?;
        let session_id = match reply_rx.recv().await {
            Some(Ok(Value::String(s))) => s,
            Some(Err(e)) => return Err(io::Error::other(e.message)),
            _ => return Err(io::Error::other("register session failed")),
        };

        let shared = SharedStream::new(self.stream);
        let mut writer = WsWriter::new(shared.clone());
        let mut reader = WsReader::new(shared, leftover);

        glommio::spawn_local(async move {
            while let Some(msg) = out_rx.recv().await {
                let ok = match msg {
                    OutMsg::Text(text) => writer.write_text(&text).await.is_ok(),
                    OutMsg::Pong(payload) => writer.write_pong(&payload).await.is_ok(),
                    OutMsg::Close => {
                        let _ = writer.write_close(1000, "bye").await;
                        false
                    }
                };
                if !ok {
                    break;
                }
            }
        })
        .detach();

        loop {
            let frame = match reader.read_frame().await {
                Ok(frame) => frame,
                Err(_) => break,
            };
            match frame {
                Frame::Text(data) => {
                    let request = match serde_json::from_slice::<protocol::Request>(&data) {
                        Ok(r) => r,
                        Err(e) => {
                            let resp = protocol::Response {
                                id: 0,
                                result: None,
                                error: Some(protocol::ErrorObj::new(
                                    protocol::PARSE_ERROR,
                                    format!("parse error: {e}"),
                                )),
                            };
                            let _ = out_tx.try_send(OutMsg::Text(
                                serde_json::to_string(&resp).unwrap_or_default(),
                            ));
                            continue;
                        }
                    };
                    // Apply Fetch decisions on the side-channel so a paused
                    // navigation can unblock without waiting for browser_loop.
                    if let Some(fetch) = &self.fetch {
                        apply_fetch_decision(fetch, &request);
                    }
                    // Fetch continue/fulfill/fail must not wait on the browser
                    // task: that task is often blocked inside `wait_fetch_decision`
                    // for the in-flight navigate. Respond here and skip the
                    // round-trip (server handlers are no-ops anyway).
                    if matches!(
                        request.method.as_str(),
                        "Fetch.continueRequest" | "Fetch.fulfillRequest" | "Fetch.failRequest"
                    ) {
                        let resp = protocol::Response {
                            id: request.id,
                            result: Some(serde_json::json!({})),
                            error: None,
                        };
                        let _ = out_tx.try_send(OutMsg::Text(
                            serde_json::to_string(&resp).unwrap_or_default(),
                        ));
                        continue;
                    }
                    // Per-command reply channel + detached waiter so the reader
                    // keeps accepting frames (e.g. Fetch.* while navigate awaits).
                    let (cmd_reply_tx, cmd_reply_rx) = new_reply_channel();
                    if let Err(e) = enqueue(
                        &self.cmd,
                        &self.config,
                        CdpRequest::InvokeCommand {
                            session_id: session_id.clone(),
                            target_id: target_id.clone(),
                            request: request.clone(),
                            reply: cmd_reply_tx,
                        },
                    ) {
                        let resp = protocol::Response {
                            id: request.id,
                            result: None,
                            error: Some(e),
                        };
                        let _ = out_tx.try_send(OutMsg::Text(
                            serde_json::to_string(&resp).unwrap_or_default(),
                        ));
                        continue;
                    }
                    let out_tx2 = out_tx.clone();
                    let req_id = request.id;
                    glommio::spawn_local(async move {
                        let outcome = match cmd_reply_rx.recv().await {
                            Some(r) => r,
                            None => return,
                        };
                        let resp = match outcome {
                            Ok(v) => protocol::Response {
                                id: req_id,
                                result: Some(v),
                                error: None,
                            },
                            Err(e) => protocol::Response {
                                id: req_id,
                                result: None,
                                error: Some(e),
                            },
                        };
                        if out_tx2
                            .try_send(OutMsg::Text(
                                serde_json::to_string(&resp).unwrap_or_default(),
                            ))
                            .is_err()
                        {
                            let _ = out_tx2.try_send(OutMsg::Close);
                        }
                    })
                    .detach();
                }
                Frame::Ping(payload) => {
                    let _ = out_tx.try_send(OutMsg::Pong(payload));
                }
                Frame::Close { .. } => {
                    let _ = out_tx.try_send(OutMsg::Close);
                    break;
                }
                Frame::Pong(_) | Frame::Binary(_) => {}
            }
        }
        let _ = enqueue(
            &self.cmd,
            &self.config,
            CdpRequest::DetachSession {
                session_id,
                reply: reply_tx.clone(),
            },
        );
        let _ = reply_rx.recv().await;
        Ok(())
    }

    /// Preview WebSocket: push HTML snapshots and accept node-addressed input.
    async fn preview_ws_loop(
        self,
        target_id: String,
        leftover: Vec<u8>,
        reply_tx: &Sender<Reply>,
        reply_rx: &Receiver<Reply>,
        csrf_required: bool,
        session_sid: Option<String>,
    ) -> io::Result<()> {
        let (out_tx, out_rx) = new_out_channel();
        enqueue(
            &self.cmd,
            &self.config,
            CdpRequest::RegisterPreview {
                target_id: target_id.clone(),
                out: out_tx.clone(),
                reply: reply_tx.clone(),
            },
        )
        .map_err(|e| io::Error::other(e.message))?;
        match reply_rx.recv().await {
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(io::Error::other(e.message)),
            None => return Err(io::Error::other("register preview failed")),
        }

        let shared = SharedStream::new(self.stream);
        let mut writer = WsWriter::new(shared.clone());
        let mut reader = WsReader::new(shared.clone(), leftover);

        enqueue(
            &self.cmd,
            &self.config,
            CdpRequest::PushPreviewSnapshot {
                target_id: target_id.clone(),
                reply: reply_tx.clone(),
            },
        )
        .map_err(|e| io::Error::other(e.message))?;
        match reply_rx.recv().await {
            Some(Ok(snap)) => {
                let text = serde_json::to_string(&snap).unwrap_or_default();
                writer.write_text(&text).await?;
            }
            Some(Err(e)) => return Err(io::Error::other(e.message)),
            None => return Err(io::Error::other("push preview failed")),
        }

        glommio::spawn_local(async move {
            while let Some(msg) = out_rx.recv().await {
                let ok = match msg {
                    OutMsg::Text(text) => writer.write_text(&text).await.is_ok(),
                    OutMsg::Pong(payload) => writer.write_pong(&payload).await.is_ok(),
                    OutMsg::Close => {
                        let _ = writer.write_close(1000, "bye").await;
                        false
                    }
                };
                if !ok {
                    break;
                }
            }
        })
        .detach();

        let mut csrf_ok = !csrf_required;
        let mut bound_profile: Option<String> = None;
        loop {
            let frame = match reader.read_frame().await {
                Ok(frame) => frame,
                Err(_) => break,
            };
            match frame {
                Frame::Ping(payload) => {
                    let _ = out_tx.try_send(OutMsg::Pong(payload));
                }
                Frame::Close { .. } => {
                    let _ = out_tx.try_send(OutMsg::Close);
                    break;
                }
                Frame::Binary(_) => {
                    let err = crate::preview::preview_error(
                        None,
                        "bad_frame",
                        "binary preview frames are not supported",
                        false,
                    );
                    let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                }
                Frame::Pong(_) => {}
                Frame::Text(data) => {
                    if data.len() > 64 * 1024 {
                        let err = crate::preview::preview_error(
                            None,
                            "too_large",
                            "preview command exceeds 64KiB",
                            false,
                        );
                        let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                        continue;
                    }
                    let msg = match serde_json::from_slice::<PreviewClientMsg>(&data) {
                        Ok(m) => m,
                        Err(e) => {
                            let err = crate::preview::preview_error(
                                None,
                                "bad_request",
                                &format!("invalid preview command: {e}"),
                                false,
                            );
                            let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                            continue;
                        }
                    };
                    match msg {
                        PreviewClientMsg::Hello { csrf, profile_id } => {
                            if csrf_required {
                                let token = csrf.as_deref().unwrap_or("");
                                let sid = session_sid.as_deref().unwrap_or("");
                                if !self.dashboard.csrf_ok(sid, token) {
                                    let err = crate::preview::preview_error(
                                        None,
                                        "csrf",
                                        "invalid csrf token",
                                        false,
                                    );
                                    let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                                    csrf_ok = false;
                                    continue;
                                }
                            }
                            csrf_ok = true;
                            bound_profile = profile_id;
                            let ack = crate::preview::preview_ack(0, 0);
                            let _ = out_tx.try_send(OutMsg::Text(ack.to_string()));
                        }
                        input @ PreviewClientMsg::Input { .. } => {
                            let id = match &input {
                                PreviewClientMsg::Input { id, .. } => *id,
                                _ => 0,
                            };
                            if !csrf_ok {
                                let err = crate::preview::preview_error(
                                    Some(id),
                                    "csrf",
                                    "preview hello/csrf required before input",
                                    false,
                                );
                                let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                                continue;
                            }
                            enqueue(
                                &self.cmd,
                                &self.config,
                                CdpRequest::PreviewCommand {
                                    target_id: target_id.clone(),
                                    profile_id: bound_profile.clone(),
                                    msg: input,
                                    reply: reply_tx.clone(),
                                },
                            )
                            .map_err(|e| io::Error::other(e.message))?;
                            match reply_rx.recv().await {
                                Some(Ok(v)) => {
                                    let _ = out_tx.try_send(OutMsg::Text(v.to_string()));
                                }
                                Some(Err(e)) => {
                                    let err = crate::preview::preview_error(
                                        Some(id),
                                        "server",
                                        &e.message,
                                        false,
                                    );
                                    let _ = out_tx.try_send(OutMsg::Text(err.to_string()));
                                }
                                None => {}
                            }
                        }
                    }
                }
            }
        }
        let _ = enqueue(
            &self.cmd,
            &self.config,
            CdpRequest::DetachPreview {
                target_id,
                reply: reply_tx.clone(),
            },
        );
        let _ = reply_rx.recv().await;
        Ok(())
    }
}

/// Apply Fetch.continueRequest / fulfillRequest / failRequest immediately so a
/// paused `HttpClient::fetch` can unblock without waiting for browser_loop.
fn apply_fetch_decision(fetch: &FetchIntercept, request: &protocol::Request) {
    let params = &request.params;
    let request_id = params
        .get("requestId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if request_id.is_empty() {
        return;
    }
    match request.method.as_str() {
        "Fetch.continueRequest" => {
            let url = params
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let method = params
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let headers = params.get("headers").and_then(|h| {
                h.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            Some((
                                e.get("name")?.as_str()?.to_owned(),
                                e.get("value")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
            });
            fetch.provide_decision(
                request_id,
                FetchAction::Continue {
                    url,
                    method,
                    headers,
                },
            );
        }
        "Fetch.fulfillRequest" => {
            let status = params
                .get("responseCode")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(200) as u16;
            let headers = params
                .get("responseHeaders")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            Some((
                                e.get("name")?.as_str()?.to_owned(),
                                e.get("value")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = params
                .get("body")
                .and_then(serde_json::Value::as_str)
                .map(|b| {
                    use base64::Engine;
                    Bytes::from(
                        base64::engine::general_purpose::STANDARD
                            .decode(b)
                            .unwrap_or_else(|_| b.as_bytes().to_vec()),
                    )
                })
                .unwrap_or_default();
            fetch.provide_decision(
                request_id,
                FetchAction::Fulfill {
                    status,
                    headers,
                    body,
                },
            );
        }
        "Fetch.failRequest" => {
            let error_reason = params
                .get("errorReason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Failed")
                .to_owned();
            fetch.provide_decision(request_id, FetchAction::Fail { error_reason });
        }
        _ => {}
    }
}

fn new_out_channel() -> (Sender<OutMsg>, Receiver<OutMsg>) {
    let (s, r) = glommio::channels::local_channel::new_unbounded();
    (Rc::new(s), Rc::new(r))
}

fn new_reply_channel() -> (Sender<Reply>, Receiver<Reply>) {
    let (s, r) = glommio::channels::local_channel::new_unbounded();
    (Rc::new(s), Rc::new(r))
}

/// Read the HTTP request head (up to `\r\n\r\n`) and split off any bytes that
/// already arrived for the WebSocket layer.
async fn read_http_head(
    stream: &mut glommio::net::TcpStream,
    max_header_bytes: usize,
) -> io::Result<(HttpRequest, Vec<u8>)> {
    let mut all = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during request head",
            ));
        }
        all.extend_from_slice(&tmp[..n]);
        if all.len() > max_header_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds maximum size",
            ));
        }
        if let Some(pos) = all.windows(4).position(|w| w == b"\r\n\r\n") {
            let head: Vec<u8> = all[..pos].to_vec();
            let leftover: Vec<u8> = all[pos + 4..].to_vec();
            return Ok((parse_http_head(&head)?, leftover));
        }
    }
}

fn parse_http_head(head: &[u8]) -> io::Result<HttpRequest> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty request"))?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request target"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (target.to_owned(), None),
    };
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
    })
}

fn classify(req: &HttpRequest) -> Route {
    match req.path.as_str() {
        "/json/version" => Route::Json(RequestKind::Version),
        "/json/list" | "/json" => Route::Json(RequestKind::List),
        "/json/new" => Route::Json(RequestKind::New(query_url(req.query.as_deref()))),
        "/preview" | "/preview/" => Route::PreviewIndex,
        "/health/live" => Route::Live,
        "/health/ready" => Route::Ready,
        "/metrics" => Route::Metrics,
        "/ui" | "/ui/" => Route::Ui,
        "/ui/app.js" => Route::UiAsset {
            name: "app.js".into(),
        },
        "/ui/app.css" => Route::UiAsset {
            name: "app.css".into(),
        },
        _ => {
            if let Some(rest) = req.path.strip_prefix("/json/close/") {
                Route::Json(RequestKind::Close(rest.to_owned()))
            } else if let Some(rest) = req.path.strip_prefix("/json/activate/") {
                Route::Json(RequestKind::Activate(rest.to_owned()))
            } else if let Some(id) = req.path.strip_prefix("/devtools/page/") {
                Route::Ws {
                    target_id: id.to_owned(),
                }
            } else if let Some(id) = req.path.strip_prefix("/devtools/browser/") {
                Route::Ws {
                    target_id: id.to_owned(),
                }
            } else if let Some(id) = req.path.strip_prefix("/preview/live/") {
                Route::PreviewLive {
                    target_id: id.to_owned(),
                }
            } else if let Some(id) = req.path.strip_prefix("/preview/") {
                Route::PreviewPage {
                    target_id: id.to_owned(),
                }
            } else if req.path.starts_with("/api/v1/dashboard/") {
                Route::DashboardApi {
                    path: req.path.clone(),
                }
            } else {
                Route::NotFound
            }
        }
    }
}

fn header_value<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn enqueue(cmd: &Sender<CdpRequest>, config: &Config, req: CdpRequest) -> Result<(), ErrorObj> {
    let metrics = hyperion_metrics::global();
    let depth = metrics.queue_depth.load(Ordering::Relaxed);
    if depth as usize >= config.max_queued_commands {
        metrics.admission_rejected.fetch_add(1, Ordering::Relaxed);
        return Err(ErrorObj::overloaded("too many queued commands"));
    }
    metrics.queue_depth.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = cmd.try_send(req) {
        metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
        return Err(ErrorObj::server(format!("command channel closed: {e}")));
    }
    Ok(())
}

fn bearer_token(req: &HttpRequest) -> Option<&str> {
    req.headers.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("authorization") {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        } else {
            None
        }
    })
}

fn authorized(config: &Config, dash: &DashboardState, req: &HttpRequest) -> bool {
    if !config.auth_required() {
        return true;
    }
    if let Some(tok) = bearer_token(req)
        && config.token_matches(tok)
    {
        return true;
    }
    if let Some((_, cookie)) = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        && let Some(sid) = hyperion_dashboard::session_from_cookie(cookie)
        && dash.session_ok(&sid).is_some()
    {
        return true;
    }
    false
}

fn origin_allowed(req: &HttpRequest) -> bool {
    let Some(origin) = header_value(req, "origin") else {
        return true;
    };
    let Some(host) = header_value(req, "host") else {
        return false;
    };
    let http = format!("http://{host}");
    let https = format!("https://{host}");
    origin.eq_ignore_ascii_case(&http) || origin.eq_ignore_ascii_case(&https)
}

/// Extract the `url=` query parameter, percent-decoded.
fn query_url(query: Option<&str>) -> String {
    query
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "url"))
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

async fn write_http(
    stream: &mut glommio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    write_all(stream, resp.as_bytes()).await
}

async fn write_all(stream: &mut glommio::net::TcpStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let n = stream.write(data).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write stalled"));
        }
        data = &data[n..];
    }
    Ok(())
}
