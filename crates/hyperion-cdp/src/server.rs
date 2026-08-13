//! The CDP browser task: target/session registry, command dispatch and the
//! event pump. Owned by the glommio executor thread; every command is executed
//! here, so `Page` (which is `!Send`) never leaves the thread and concurrent
//! sessions are serialized.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use hyperion_browser::{BrowserContext, InteractionOutcome, Page};
use hyperion_config::Config;
use hyperion_dashboard::DashboardState;
use hyperion_dom::{NodeData, NodeId};
use hyperion_js::EvalOutcome;
use hyperion_net::client::{FetchIntercept, HttpClientOptions, NetEvent, NetEventSink};
use hyperion_profile::{ProfileId, ProfileKind, ProfileManager};
use serde_json::{Value, json};

use crate::connection::{CdpRequest, OutMsg, Sender};
use crate::preview::{self, PreviewClientMsg};
use crate::protocol::{self, ErrorObj};

/// Configuration for the CDP server.
pub struct CdpServerOptions {
    /// Address to listen on (`127.0.0.1:0` picks an ephemeral port).
    pub bind: std::net::SocketAddr,
    /// HTTP client options; the server installs its own `net_events` sink.
    pub http: HttpClientOptions,
    pub config: Config,
}

impl Default for CdpServerOptions {
    fn default() -> Self {
        let bind = "127.0.0.1:0".parse().unwrap();
        let mut config = Config::default();
        config.bind = bind;
        config.allow_private_network = true;
        Self {
            bind,
            http: HttpClientOptions::default(),
            config,
        }
    }
}

struct BodyCache {
    map: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
    bytes: usize,
    max: usize,
}

impl BodyCache {
    fn new(max: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max: max.max(1024),
        }
    }

    fn insert(&mut self, id: String, body: Vec<u8>) {
        if let Some(old) = self.map.remove(&id) {
            self.bytes = self.bytes.saturating_sub(old.len());
            self.order.retain(|x| x != &id);
        }
        while self.bytes + body.len() > self.max && !self.order.is_empty() {
            if let Some(k) = self.order.pop_front() {
                if let Some(v) = self.map.remove(&k) {
                    self.bytes = self.bytes.saturating_sub(v.len());
                }
            }
        }
        self.bytes += body.len();
        self.map.insert(id.clone(), body);
        self.order.push_back(id);
        hyperion_metrics::global()
            .cache_bytes
            .store(self.bytes as u64, Ordering::Relaxed);
    }

    fn get(&self, id: &str) -> Option<&[u8]> {
        self.map.get(id).map(|v| v.as_slice())
    }
}

/// What a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Page,
    Browser,
}

struct Target {
    kind: TargetKind,
    url: String,
    title: String,
    page: Option<Page>,
    profile_id: String,
    last_active: Instant,
}

struct Session {
    id: String,
    target_id: String,
    out: Sender<OutMsg>,
    /// Flattened sessions carry their id on events/commands.
    flattened: bool,
    page_enabled: bool,
    runtime_enabled: bool,
    network_enabled: bool,
    dom_enabled: bool,
    log_enabled: bool,
    fetch_enabled: bool,
}

impl Session {
    fn new(id: String, target_id: String, out: Sender<OutMsg>, flattened: bool) -> Self {
        Self {
            id,
            target_id,
            out,
            flattened,
            page_enabled: false,
            runtime_enabled: false,
            network_enabled: false,
            dom_enabled: false,
            log_enabled: false,
            fetch_enabled: false,
        }
    }
}

/// Events produced by the engine (network + console) and drained by the
/// browser task into protocol events. The producers live on the executor
/// thread too but are reached through a std channel so the sink closures can
/// be `Send + Sync`.
enum ServerEvent {
    Net(NetEvent),
    Console {
        target_id: String,
        kind: String,
        args: Vec<String>,
    },
}

pub struct CdpServer {
    listener: Option<glommio::net::TcpListener>,
    addr: std::net::SocketAddr,
    context: Rc<BrowserContext>,
    targets: HashMap<String, Target>,
    sessions: HashMap<String, Session>,
    /// Live-preview subscribers keyed by target id.
    previews: HashMap<String, Vec<Sender<OutMsg>>>,
    event_tx: mpsc::Sender<ServerEvent>,
    event_rx: mpsc::Receiver<ServerEvent>,
    browser_target_id: String,
    next_target_id: u64,
    next_session_id: u64,
    /// Response bodies by request id, for `Network.getResponseBody`.
    response_bodies: BodyCache,
    fetch: FetchIntercept,
    /// Approximate layout metrics (Emulation.setDeviceMetricsOverride).
    viewport: (f64, f64, f64),
    lifecycle_events: bool,
    /// Sessions that should receive Fetch.requestPaused immediately (side-channel).
    fetch_emitters: Rc<RefCell<HashMap<String, Sender<OutMsg>>>>,
    config: Config,
    profiles: ProfileManager,
    contexts: HashMap<String, Rc<BrowserContext>>,
    default_profile: ProfileId,
    shutdown: Arc<AtomicBool>,
    dashboard: DashboardState,
    last_idle_sweep: Instant,
    active_navigations: usize,
    active_evals: usize,
}

impl CdpServer {
    pub fn new(options: CdpServerOptions) -> std::io::Result<Self> {
        let listener = glommio::net::TcpListener::bind(options.bind)?;
        let addr = listener.local_addr()?;

        let (event_tx, event_rx) = mpsc::channel::<ServerEvent>();
        let sink_tx = event_tx.clone();
        let net_sink: NetEventSink = Arc::new(move |ne| {
            let _ = sink_tx.send(ServerEvent::Net(ne));
        });
        let fetch = FetchIntercept::new();
        let fetch_emitters: Rc<RefCell<HashMap<String, Sender<OutMsg>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let emitters_for_pause = fetch_emitters.clone();
        let fetch_paused: hyperion_net::client::FetchPausedSink =
            Rc::new(move |id, url, method, headers| {
                let msg = json!({
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": id.to_string(),
                        "request": {
                            "url": url,
                            "method": method,
                            "headers": {
                                // flatten header map best-effort
                            },
                            "initialPriority": "Low",
                            "referrerPolicy": "no-referrer-when-downgrade",
                        },
                        "frameId": "",
                        "resourceType": "Document",
                    }
                });
                // Attach headers
                let mut header_map = serde_json::Map::new();
                for (k, v) in headers.iter() {
                    header_map.insert(
                        k.as_str().to_owned(),
                        v.to_str().unwrap_or("").to_owned().into(),
                    );
                }
                let mut msg = msg;
                msg["params"]["request"]["headers"] = Value::Object(header_map);
                let text = serde_json::to_string(&msg).unwrap_or_default();
                for out in emitters_for_pause.borrow().values() {
                    let _ = out.try_send(OutMsg::Text(text.clone()));
                }
            });
        let mut http = options.http;
        http.allow_private_network = options.config.allow_private_network;
        http.max_body_bytes = options.config.max_body_bytes;
        http.fetch_pause_timeout = options.config.fetch_pause_timeout;
        http.per_origin_connections = options.config.per_origin_connections;
        http.net_events = Some(net_sink);
        http.fetch_intercept = Some(fetch.clone());
        http.fetch_paused = Some(fetch_paused);

        hyperion_js::configure_v8_pool(
            options.config.v8_pool_size,
            options.config.v8_heap_mb,
            options.config.max_remote_objects,
            options.config.eval_deadline,
        );

        let mut profiles = build_profile_manager(&options.config);
        let default_profile = profiles.create_ephemeral("default");
        let context = Rc::new(BrowserContext::new(http));
        *context.profile_id.borrow_mut() = default_profile.as_str().to_owned();
        let mut contexts = HashMap::new();
        contexts.insert(default_profile.as_str().to_owned(), context.clone());

        let browser_target_id = "browser".to_owned();
        let mut targets = HashMap::new();
        targets.insert(
            browser_target_id.clone(),
            Target {
                kind: TargetKind::Browser,
                url: "about:blank".to_owned(),
                title: "Hyperion".to_owned(),
                page: None,
                profile_id: default_profile.as_str().to_owned(),
                last_active: Instant::now(),
            },
        );

        let dashboard = DashboardState::new(options.config.clone());
        Ok(Self {
            listener: Some(listener),
            addr,
            context,
            targets,
            sessions: HashMap::new(),
            previews: HashMap::new(),
            event_tx,
            event_rx,
            browser_target_id,
            next_target_id: 1,
            next_session_id: 1,
            response_bodies: BodyCache::new(options.config.max_response_cache_bytes),
            fetch,
            viewport: (980.0, 1024.0, 1.0),
            lifecycle_events: false,
            fetch_emitters,
            config: options.config,
            profiles,
            contexts,
            default_profile,
            shutdown: Arc::new(AtomicBool::new(false)),
            dashboard,
            last_idle_sweep: Instant::now(),
            active_navigations: 0,
            active_evals: 0,
        })
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Run forever: spawn the browser task and accept connections on this
    /// executor thread. Must be called from a glommio executor.
    pub async fn run(mut self) -> std::io::Result<()> {
        let (cmd_tx, cmd_rx) = glommio::channels::local_channel::new_unbounded::<CdpRequest>();
        let cmd_tx = Rc::new(cmd_tx);
        let cmd_rx = Rc::new(cmd_rx);
        let accept_cmd = cmd_tx.clone();
        let fetch = self.fetch.clone();
        let config = self.config.clone();
        let dashboard = self.dashboard.clone();
        let shutdown = self.shutdown.clone();
        let listener = self.listener.take().expect("server already running");

        glommio::spawn_local(async move {
            self.browser_loop(cmd_rx).await;
        })
        .detach();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let stream = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "cdp accept error");
                    continue;
                }
            };
            let cmd = accept_cmd.clone();
            let fetch = fetch.clone();
            let config = config.clone();
            let dashboard = dashboard.clone();
            glommio::spawn_local(async move {
                if let Err(e) = crate::connection::ConnectionTask::new(
                    stream,
                    cmd,
                    Some(fetch),
                    config,
                    dashboard,
                )
                .run()
                .await
                {
                    tracing::debug!(error = %e, "cdp connection error");
                }
            })
            .detach();
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Browser task
    // ------------------------------------------------------------------

    async fn browser_loop(&mut self, cmd_rx: crate::connection::Receiver<CdpRequest>) {
        let _ = self.profiles.heartbeat();
        self.refresh_gauges();
        while let Some(req) = cmd_rx.recv().await {
            self.pump_events();
            self.sweep_idle();
            self.handle(req).await;
            hyperion_metrics::global()
                .queue_depth
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                })
                .ok();
            self.pump_events();
            if self.shutdown.load(Ordering::Relaxed) {
                let _ = self.profiles.flush_all();
                break;
            }
        }
    }

    async fn handle(&mut self, req: CdpRequest) {
        match req {
            CdpRequest::Version { reply } => Self::send(reply, Ok(self.version())),
            CdpRequest::ListTargets { reply } => Self::send(reply, Ok(self.list_targets())),
            CdpRequest::NewTarget { url, reply } => {
                let result = self.new_target(&url).await;
                Self::send(reply, result);
            }
            CdpRequest::CloseTarget { id, reply } => Self::send(reply, Ok(self.close_target(&id))),
            CdpRequest::Activate { reply, .. } => Self::send(reply, Ok(Value::Null)),
            CdpRequest::RegisterSession {
                target_id,
                out,
                reply,
            } => {
                if !self.targets.contains_key(&target_id) {
                    Self::send(reply, Err(ErrorObj::server("no such target")));
                    return;
                }
                if self.sessions.len() >= self.config.max_sessions {
                    hyperion_metrics::global()
                        .admission_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    Self::send(reply, Err(ErrorObj::overloaded("too many sessions")));
                    return;
                }
                let session_id = self.next_session_id();
                self.sessions.insert(
                    session_id.clone(),
                    Session::new(session_id.clone(), target_id, out, false),
                );
                self.store_lifecycle_gauges();
                Self::send(reply, Ok(json!(session_id)));
            }
            CdpRequest::InvokeCommand {
                session_id,
                target_id,
                request,
                reply,
            } => {
                let started = Instant::now();
                hyperion_metrics::global()
                    .commands
                    .fetch_add(1, Ordering::Relaxed);
                let result = self.dispatch(&session_id, &target_id, &request).await;
                let ms = started.elapsed().as_millis() as u64;
                hyperion_metrics::global()
                    .cmd_latency_ms_sum
                    .fetch_add(ms, Ordering::Relaxed);
                hyperion_metrics::global()
                    .cmd_latency_ms_count
                    .fetch_add(1, Ordering::Relaxed);
                Self::send(reply, result);
            }
            CdpRequest::DetachSession { session_id, reply } => {
                self.sessions.remove(&session_id);
                self.store_lifecycle_gauges();
                Self::send(reply, Ok(Value::Null));
            }
            CdpRequest::RegisterPreview {
                target_id,
                out,
                reply,
            } => {
                if !self.targets.contains_key(&target_id) {
                    Self::send(reply, Err(ErrorObj::server("no such target")));
                    return;
                }
                let n: usize = self.previews.values().map(|v| v.len()).sum();
                if n >= self.config.max_previews {
                    hyperion_metrics::global()
                        .admission_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    Self::send(reply, Err(ErrorObj::overloaded("too many previews")));
                    return;
                }
                self.previews
                    .entry(target_id.clone())
                    .or_default()
                    .push(out);
                self.store_lifecycle_gauges();
                // Snapshot is pushed after the preview writer is running
                // (`PushPreviewSnapshot`) so the first frame is not stuck in
                // the out-channel before anyone drains it.
                Self::send(reply, Ok(Value::Null));
            }
            CdpRequest::PushPreviewSnapshot { target_id, reply } => {
                match self.preview_snapshot_value(&target_id) {
                    Some(v) => Self::send(reply, Ok(v)),
                    None => Self::send(reply, Err(ErrorObj::server("no preview snapshot"))),
                }
            }
            CdpRequest::DetachPreview { target_id, reply } => {
                self.previews.remove(&target_id);
                self.store_lifecycle_gauges();
                Self::send(reply, Ok(Value::Null));
            }
            CdpRequest::PreviewCommand {
                target_id,
                profile_id,
                msg,
                reply,
            } => {
                let result = self
                    .handle_preview_command(&target_id, profile_id.as_deref(), msg)
                    .await;
                Self::send(reply, result);
            }
            CdpRequest::Shutdown { reply } => {
                self.begin_shutdown();
                Self::send(reply, Ok(json!({ "ok": true })));
            }
            CdpRequest::Dashboard {
                path,
                query,
                method,
                reply,
            } => {
                let result = self.dashboard_api(&path, query.as_deref(), &method).await;
                Self::send(reply, result);
            }
        }
    }

    fn send(tx: Sender<Result<Value, ErrorObj>>, value: Result<Value, ErrorObj>) {
        let _ = tx.try_send(value);
    }

    // ------------------------------------------------------------------
    // Command dispatch
    // ------------------------------------------------------------------

    async fn dispatch(
        &mut self,
        conn_session: &str,
        conn_target: &str,
        req: &protocol::Request,
    ) -> Result<Value, ErrorObj> {
        let root_out = self.sessions.get(conn_session).map(|s| s.out.clone());
        let target_id = self.resolve_target(conn_target, req)?;
        let method = req.method.as_str();
        // The session a domain enable/disable applies to: the flattened session
        // when the request carries one, otherwise the connection's root session.
        let eff_session = req
            .session_id
            .clone()
            .unwrap_or_else(|| conn_session.to_owned());

        // Domain commands that only make sense on a page target.
        if is_page_domain(method)
            && self.targets.get(&target_id).map(|t| t.kind) != Some(TargetKind::Page)
        {
            return Err(ErrorObj::server("Not attached to a page"));
        }

        match method {
            // Settings that are accepted but have no observable effect yet.
            "Runtime.runIfWaitingForDebugger"
            | "Page.setInterceptFileChooserDialog"
            | "Network.setBlockedURLs"
            | "Network.setRequestInterception"
            | "Emulation.setTouchEmulationEnabled"
            | "Emulation.setFocusEmulationEnabled"
            | "Emulation.setDefaultBackgroundColorOverride"
            | "Performance.enable"
            | "Profiler.enable"
            | "Memory.enable"
            | "Browser.close"
            | "Page.bringToFront"
            | "Page.setBypassCSP"
            | "DOM.scrollIntoViewIfNeeded"
            | "DOM.hideHighlight"
            | "DOM.highlightNode"
            | "Overlay.enable"
            | "Overlay.disable"
            | "Security.enable"
            | "Security.disable"
            | "Inspector.enable"
            | "Inspector.disable" => Ok(json!({})),

            "Runtime.enable" => self.runtime_enable(&eff_session, &target_id),
            "Runtime.disable" => self.runtime_disable(&eff_session),
            "Runtime.evaluate" => self.runtime_evaluate(&target_id, &req.params),
            "Runtime.callFunctionOn" => self.runtime_call_function_on(&target_id, &req.params),
            "Runtime.getProperties" => self.runtime_get_properties(&target_id, &req.params),
            "Runtime.awaitPromise" => self.runtime_await_promise(&target_id, &req.params),
            "Runtime.releaseObject" => self.runtime_release_object(&target_id, &req.params),
            "Runtime.releaseObjectGroup" => {
                self.runtime_release_object_group(&target_id, &req.params)
            }

            "Page.enable" => self.page_enable(&eff_session, &target_id),
            "Page.disable" => self.page_disable(&eff_session),
            "Page.navigate" => self.page_navigate(&target_id, &req.params).await,
            "Page.reload" => self.page_reload(&target_id).await,
            "Page.close" => self.page_close(&target_id),
            "Page.getFrameTree" => self.page_get_frame_tree(&target_id),
            "Page.getResourceTree" => self.page_get_resource_tree(&target_id),
            "Page.getNavigationHistory" => self.page_get_navigation_history(&target_id),
            "Page.getLayoutMetrics" => self.page_get_layout_metrics(),
            "Page.setLifecycleEventsEnabled" => {
                self.lifecycle_events = req
                    .params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Ok(json!({}))
            }
            "Page.addScriptToEvaluateOnNewDocument" => self.page_add_init_script(&req.params),
            "Page.removeScriptToEvaluateOnNewDocument" => self.page_remove_init_script(&req.params),
            "Page.createIsolatedWorld" => self.page_create_isolated_world(&target_id, &req.params),

            "Network.enable" => self.network_enable(&eff_session),
            "Network.disable" => self.network_disable(&eff_session),
            "Network.getResponseBody" => self.network_get_response_body(&req.params),
            "Network.setCacheDisabled" => Ok(json!({})),
            "Network.setExtraHTTPHeaders" => self.network_set_extra_headers(&req.params),
            "Network.setCookie" => self.network_set_cookie(&req.params),
            "Network.getCookies" => self.network_get_cookies(&req.params),
            "Network.getAllCookies" => self.network_get_all_cookies(),
            "Network.deleteCookies" => self.network_delete_cookies(&req.params),
            "Network.clearBrowserCookies" => {
                self.context.client().cookies_mut().clear();
                Ok(json!({}))
            }

            "Fetch.enable" => self.fetch_enable(&eff_session, &req.params),
            "Fetch.disable" => self.fetch_disable(&eff_session),
            "Fetch.continueRequest" | "Fetch.fulfillRequest" | "Fetch.failRequest" => {
                // Decision already applied on the connection side-channel.
                Ok(json!({}))
            }

            "DOM.enable" => self.dom_enable(&eff_session, &target_id),
            "DOM.disable" => self.dom_disable(&eff_session),
            "DOM.getDocument" => self.dom_get_document(&target_id, &req.params),
            "DOM.querySelector" => self.dom_query_selector(&target_id, &req.params),
            "DOM.querySelectorAll" => self.dom_query_selector_all(&target_id, &req.params),
            "DOM.requestChildNodes" => self.dom_request_child_nodes(&target_id, &req.params),
            "DOM.getOuterHTML" => self.dom_get_outer_html(&target_id, &req.params),
            "DOM.setOuterHTML" => self.dom_set_outer_html(&target_id, &req.params),
            "DOM.getAttributes" => self.dom_get_attributes(&target_id, &req.params),
            "DOM.setAttribute" => self.dom_set_attribute(&target_id, &req.params),
            "DOM.removeAttribute" => self.dom_remove_attribute(&target_id, &req.params),
            "DOM.setNodeValue" => self.dom_set_node_value(&target_id, &req.params),
            "DOM.removeNode" => self.dom_remove_node(&target_id, &req.params),
            "DOM.focus" => self.dom_focus(&target_id, &req.params),
            "DOM.resolveNode" => self.dom_resolve_node_obj(&target_id, &req.params),
            "DOM.describeNode" => self.dom_describe_node(&target_id, &req.params),
            "DOM.getBoxModel" => self.dom_get_box_model(&target_id, &req.params),
            "DOM.getContentQuads" => self.dom_get_content_quads(&target_id, &req.params),

            "Input.dispatchKeyEvent" => self.input_key(&target_id, &req.params),
            "Input.insertText" => self.input_insert_text(&target_id, &req.params),
            "Input.dispatchMouseEvent" => self.input_mouse(&target_id, &req.params).await,

            "Log.enable" => self.log_enable(&eff_session),
            "Log.disable" => self.log_disable(&eff_session),

            "Emulation.setDeviceMetricsOverride" => self.emulation_set_metrics(&req.params),
            "Emulation.clearDeviceMetricsOverride" => self.emulation_clear_metrics(),
            "Emulation.setUserAgentOverride" => self.emulation_set_ua(&req.params),

            "Target.getTargets" => Ok(self.target_get_targets()),
            "Target.getTargetInfo" => self.target_get_target_info(&target_id, &req.params),
            "Target.createTarget" => self.target_create(&req.params).await,
            "Target.attachToTarget" => self.target_attach(conn_session, &req.params),
            "Target.detachFromTarget" => self.target_detach(&req.params),
            "Target.setDiscoverTargets" => {
                self.target_set_discover_targets(root_out.as_ref(), &req.params)
            }
            "Target.setAutoAttach" => self.target_set_auto_attach(root_out.as_ref(), &req.params),
            "Target.createBrowserContext" => self.target_create_browser_context(),
            "Target.disposeBrowserContext" => self.target_dispose_browser_context(&req.params),
            "Target.getBrowserContexts" => Ok(json!({
                "browserContextIds": self.contexts.keys().cloned().collect::<Vec<_>>()
            })),

            "Hyperion.Profile.list" => self.profile_list(&req.params),
            "Hyperion.Profile.create" => self.profile_create(&req.params),
            "Hyperion.Profile.open" => self.profile_open(&req.params),
            "Hyperion.Profile.close" => self.profile_close(&req.params),
            "Hyperion.Profile.delete" => self.profile_delete(&req.params),
            "Hyperion.Profile.flush" => self.profile_flush(&req.params),

            "Browser.getVersion" => Ok(self.version()),
            "Browser.getWindowForTarget" => Ok(json!({ "windowId": 1, "bounds": {
                "left": 0, "top": 0, "width": self.viewport.0 as i64, "height": self.viewport.1 as i64, "windowState": "normal"
            }})),
            "Browser.setWindowBounds" => Ok(json!({})),
            _ => Err(ErrorObj::method_not_found(method)),
        }
    }

    fn resolve_target(
        &self,
        conn_target: &str,
        req: &protocol::Request,
    ) -> Result<String, ErrorObj> {
        match &req.session_id {
            Some(sid) => self
                .sessions
                .get(sid)
                .map(|s| s.target_id.clone())
                .ok_or_else(|| ErrorObj::server("session not found")),
            None => Ok(conn_target.to_owned()),
        }
    }

    // ------------------------------------------------------------------
    // Domain enable/disable
    // ------------------------------------------------------------------

    fn set_session_flag(&mut self, session_id: &str, f: impl FnOnce(&mut Session)) {
        if let Some(s) = self.sessions.get_mut(session_id) {
            f(s);
        }
    }

    fn runtime_enable(&mut self, session_id: &str, target_id: &str) -> Result<Value, ErrorObj> {
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.runtime_enabled = true;
        }
        // Announce the default execution context so clients can evaluate.
        if self
            .targets
            .get(target_id)
            .map(|t| t.page.is_some())
            .unwrap_or(false)
        {
            let context = json!({
                "id": 1,
                "origin": self
                    .targets
                    .get(target_id)
                    .map(|t| url_origin(&t.url))
                    .unwrap_or_default(),
                "name": "",
                "uniqueId": format!("{target_id}/1"),
                "auxData": { "isDefault": true, "type": "default", "frameId": target_id },
            });
            self.emit_gated(
                target_id,
                "Runtime.executionContextCreated",
                json!({ "context": context }),
                |s| s.runtime_enabled,
            );
        }
        Ok(json!({}))
    }

    fn runtime_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.runtime_enabled = false);
        Ok(json!({}))
    }

    fn page_enable(&mut self, session_id: &str, target_id: &str) -> Result<Value, ErrorObj> {
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.page_enabled = true;
        }
        // Announce frames that already exist, as Chrome does on Page.enable.
        if let Some(target) = self.targets.get(target_id)
            && target.page.is_some()
        {
            self.emit_gated(
                target_id,
                "Page.frameNavigated",
                json!({
                    "frame": {
                        "id": target_id,
                        "loaderId": target_id,
                        "url": target.url,
                        "securityOrigin": "",
                        "mimeType": "text/html",
                        "name": "",
                    },
                    "type": "Navigation",
                }),
                |s| s.page_enabled,
            );
        }
        Ok(json!({}))
    }

    fn page_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.page_enabled = false);
        Ok(json!({}))
    }

    fn network_enable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.network_enabled = true);
        Ok(json!({}))
    }

    fn network_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.network_enabled = false);
        Ok(json!({}))
    }

    fn dom_enable(&mut self, session_id: &str, target_id: &str) -> Result<Value, ErrorObj> {
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.dom_enabled = true;
        }
        if self
            .targets
            .get(target_id)
            .map(|t| t.page.is_some())
            .unwrap_or(false)
        {
            self.emit_gated(target_id, "DOM.documentUpdated", json!({}), |s| {
                s.dom_enabled
            });
        }
        Ok(json!({}))
    }

    fn dom_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.dom_enabled = false);
        Ok(json!({}))
    }

    fn log_enable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.log_enabled = true);
        Ok(json!({}))
    }

    fn log_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.log_enabled = false);
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // Runtime
    // ------------------------------------------------------------------

    fn runtime_evaluate(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        self.admit_active(self.active_evals, self.config.max_active_evals, "evals")?;
        self.active_evals += 1;
        hyperion_metrics::global()
            .evals
            .fetch_add(1, Ordering::Relaxed);
        let result = self.runtime_evaluate_inner(target_id, params);
        self.active_evals = self.active_evals.saturating_sub(1);
        result
    }

    fn runtime_evaluate_inner(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let target = self
            .targets
            .get_mut(target_id)
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        let page = target
            .page
            .as_mut()
            .ok_or_else(|| ErrorObj::server("Page has not been navigated"))?;
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing expression"))?;
        let return_by_value = params
            .get("returnByValue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let before_gen = page.bridge_generation();
        let outcome = page
            .evaluate_outcome_ex(expression, return_by_value)
            .map_err(ErrorObj::server)?;
        let after_gen = page.bridge_generation();

        let mut result = json!({ "result": remote_object_from_outcome(&outcome, return_by_value) });
        if outcome.is_exception {
            let err_obj = json!({
                "type": "object",
                "subtype": "error",
                "className": "Error",
                "description": outcome.exception.clone().unwrap_or_default(),
            });
            result["result"] = err_obj.clone();
            result["exceptionDetails"] = json!({
                "text": outcome.exception.clone().unwrap_or_default(),
                "exception": err_obj,
                "lineNumber": 0,
                "columnNumber": 0,
            });
        }
        if after_gen != before_gen {
            self.push_preview_snapshot(target_id);
        }
        Ok(result)
    }

    fn runtime_call_function_on(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let page = self.page_mut(target_id)?;
        let object_id = params
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing objectId"))?;
        let function_declaration = params
            .get("functionDeclaration")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing functionDeclaration"))?;
        let return_by_value = params
            .get("returnByValue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let arguments_json: Vec<String> = params
            .get("arguments")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|a| {
                        if let Some(oid) = a.get("objectId").and_then(Value::as_str) {
                            // Pass a sentinel; call_function_on currently expects JSON values.
                            format!(r#"{{"__objectId":"{oid}"}}"#)
                        } else if let Some(v) = a.get("value") {
                            v.to_string()
                        } else if a.get("unserializableValue").is_some() {
                            "null".to_owned()
                        } else {
                            "null".to_owned()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let outcome = page
            .call_function_on(
                object_id,
                function_declaration,
                &arguments_json,
                return_by_value,
            )
            .map_err(ErrorObj::server)?;
        Ok(json!({ "result": remote_object_from_outcome(&outcome, return_by_value) }))
    }

    fn runtime_get_properties(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let page = self.page_mut(target_id)?;
        let object_id = params
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing objectId"))?;
        let own = params
            .get("ownProperties")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let props = page
            .get_properties(object_id, own)
            .map_err(ErrorObj::server)?;
        let result: Vec<Value> = props
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "value": remote_object_from_outcome(&p.value, false),
                    "writable": p.writable,
                    "configurable": p.configurable,
                    "enumerable": p.enumerable,
                    "isOwn": p.is_own,
                })
            })
            .collect();
        Ok(json!({ "result": result }))
    }

    fn runtime_await_promise(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let page = self.page_mut(target_id)?;
        let object_id = params
            .get("promiseObjectId")
            .or_else(|| params.get("objectId"))
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing promiseObjectId"))?;
        let return_by_value = params
            .get("returnByValue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = page
            .await_promise(object_id, return_by_value)
            .map_err(ErrorObj::server)?;
        let mut result = json!({ "result": remote_object_from_outcome(&outcome, return_by_value) });
        if outcome.is_exception {
            result["exceptionDetails"] = json!({
                "text": outcome.exception.clone().unwrap_or_default(),
                "lineNumber": 0,
                "columnNumber": 0,
            });
        }
        Ok(result)
    }

    fn runtime_release_object(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let page = self.page_mut(target_id)?;
        let object_id = params
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing objectId"))?;
        page.release_object(object_id).map_err(ErrorObj::server)?;
        Ok(json!({}))
    }

    fn runtime_release_object_group(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let page = self.page_mut(target_id)?;
        let group = params
            .get("objectGroup")
            .and_then(Value::as_str)
            .unwrap_or("");
        page.release_object_group(group).map_err(ErrorObj::server)?;
        Ok(json!({}))
    }

    fn page_mut(&mut self, target_id: &str) -> Result<&mut Page, ErrorObj> {
        self.targets
            .get_mut(target_id)
            .and_then(|t| t.page.as_mut())
            .ok_or_else(|| ErrorObj::server("Page has not been navigated"))
    }

    fn page_ref(&self, target_id: &str) -> Result<&Page, ErrorObj> {
        self.targets
            .get(target_id)
            .and_then(|t| t.page.as_ref())
            .ok_or_else(|| ErrorObj::server("Page has not been navigated"))
    }

    // ------------------------------------------------------------------
    // Page
    // ------------------------------------------------------------------

    async fn page_navigate(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        self.admit_active(
            self.active_navigations,
            self.config.max_active_navigations,
            "navigations",
        )?;
        self.active_navigations += 1;
        hyperion_metrics::global()
            .targets_active
            .store(self.active_navigations as u64, Ordering::Relaxed);
        let started = Instant::now();
        let result = self.page_navigate_inner(target_id, params).await;
        let ms = started.elapsed().as_millis() as u64;
        hyperion_metrics::global()
            .nav_latency_ms_sum
            .fetch_add(ms, Ordering::Relaxed);
        hyperion_metrics::global()
            .nav_latency_ms_count
            .fetch_add(1, Ordering::Relaxed);
        self.active_navigations = self.active_navigations.saturating_sub(1);
        hyperion_metrics::global()
            .targets_active
            .store(self.active_navigations as u64, Ordering::Relaxed);
        result
    }

    async fn page_navigate_inner(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing url"))?;
        {
            let target = self
                .targets
                .get(target_id)
                .ok_or_else(|| ErrorObj::server("no such target"))?;
            if target.kind != TargetKind::Page {
                return Err(ErrorObj::server("Not attached to a page"));
            }
        }
        self.context
            .client()
            .set_target_id(Some(target_id.to_owned()));
        if let Some(t) = self.targets.get_mut(target_id) {
            t.last_active = Instant::now();
        }
        // The old context is gone once the new document starts loading.
        self.emit_gated(
            target_id,
            "Runtime.executionContextsCleared",
            json!({}),
            |s| s.runtime_enabled,
        );
        let profile_id = self
            .targets
            .get(target_id)
            .map(|t| t.profile_id.clone())
            .unwrap_or_default();
        let context = self
            .contexts
            .get(&profile_id)
            .cloned()
            .unwrap_or_else(|| self.context.clone());
        context.client().set_target_id(Some(target_id.to_owned()));
        let url = url.to_owned();
        let result = context.navigate(&url).await;
        match result {
            Ok(page) => {
                // Flush the Network.* events produced by this navigation before
                // announcing the new document, so a client sees the request
                // lifecycle before loadEventFired.
                self.pump_events();
                self.install_page(target_id, page);
                self.emit_gated(target_id, "DOM.documentUpdated", json!({}), |s| {
                    s.dom_enabled
                });
                self.emit_gated(
                    target_id,
                    "Page.frameNavigated",
                    json!({
                        "frame": {
                            "id": target_id,
                            "loaderId": target_id,
                            "url": url,
                            "securityOrigin": "",
                            "mimeType": "text/html",
                            "name": "",
                        },
                        "type": "Navigation",
                    }),
                    |s| s.page_enabled,
                );
                self.emit_gated(
                    target_id,
                    "Page.loadEventFired",
                    json!({ "timestamp": now_ts() }),
                    |s| s.page_enabled,
                );
                self.emit_gated(
                    target_id,
                    "Page.frameStoppedLoading",
                    json!({ "frameId": target_id }),
                    |s| s.page_enabled,
                );
                self.emit_gated(
                    target_id,
                    "Runtime.executionContextCreated",
                    json!({
                        "context": {
                            "id": 1,
                            "origin": url_origin(&url),
                            "name": "",
                            "uniqueId": format!("{target_id}/1"),
                            "auxData": { "isDefault": true, "type": "default", "frameId": target_id },
                        }
                    }),
                    |s| s.runtime_enabled,
                );
                Ok(json!({ "frameId": target_id }))
            }
            Err(e) => Ok(json!({ "frameId": target_id, "errorText": e.to_string() })),
        }
    }

    async fn page_reload(&mut self, target_id: &str) -> Result<Value, ErrorObj> {
        let url = self
            .targets
            .get(target_id)
            .map(|t| t.url.clone())
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        self.page_navigate(target_id, &json!({ "url": url })).await
    }

    fn page_close(&mut self, target_id: &str) -> Result<Value, ErrorObj> {
        self.close_target(target_id);
        Ok(json!({}))
    }

    fn page_get_frame_tree(&self, target_id: &str) -> Result<Value, ErrorObj> {
        let target = self
            .targets
            .get(target_id)
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        Ok(json!({
            "frameTree": {
                "frame": {
                    "id": target_id,
                    "loaderId": target_id,
                    "url": target.url,
                    "domainAndRegistry": url_domain(&target.url),
                    "securityOrigin": "",
                    "mimeType": "text/html",
                },
                "childFrames": [],
            }
        }))
    }

    fn page_get_resource_tree(&self, target_id: &str) -> Result<Value, ErrorObj> {
        let target = self
            .targets
            .get(target_id)
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        Ok(json!({
            "frameTree": {
                "frame": {
                    "id": target_id,
                    "loaderId": target_id,
                    "url": target.url,
                    "domainAndRegistry": url_domain(&target.url),
                    "securityOrigin": "",
                    "mimeType": "text/html",
                },
                "resources": [{
                    "url": target.url,
                    "type": "Document",
                    "mimeType": "text/html",
                }],
                "childFrames": [],
            }
        }))
    }

    fn page_get_navigation_history(&self, target_id: &str) -> Result<Value, ErrorObj> {
        let target = self
            .targets
            .get(target_id)
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        Ok(json!({
            "currentIndex": 0,
            "entries": [{
                "id": 0,
                "url": target.url,
                "userTypedURL": target.url,
                "title": target.title,
            }],
        }))
    }

    fn page_get_layout_metrics(&self) -> Result<Value, ErrorObj> {
        let (w, h, dpr) = self.viewport;
        Ok(json!({
            "layoutViewport": { "pageX": 0, "pageY": 0, "clientWidth": w as i64, "clientHeight": h as i64 },
            "visualViewport": { "offsetX": 0, "offsetY": 0, "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h, "scale": dpr, "zoom": 1 },
            "contentSize": { "x": 0, "y": 0, "width": w, "height": h },
            "cssLayoutViewport": { "pageX": 0, "pageY": 0, "clientWidth": w as i64, "clientHeight": h as i64 },
            "cssVisualViewport": { "offsetX": 0, "offsetY": 0, "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h, "scale": dpr, "zoom": 1 },
            "cssContentSize": { "x": 0, "y": 0, "width": w, "height": h },
        }))
    }

    fn page_add_init_script(&self, params: &Value) -> Result<Value, ErrorObj> {
        let source = params
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing source"))?;
        let id = self.context.add_init_script(source);
        Ok(json!({ "identifier": id }))
    }

    fn page_remove_init_script(&self, params: &Value) -> Result<Value, ErrorObj> {
        if let Some(id) = params.get("identifier").and_then(Value::as_str) {
            self.context.remove_init_script(id);
        }
        Ok(json!({}))
    }

    fn page_create_isolated_world(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let _ = params;
        // Single world for now; return a distinct context id.
        let page = self.page_ref(target_id)?;
        let _ = page;
        Ok(json!({ "executionContextId": 2 }))
    }

    // ------------------------------------------------------------------
    // Network
    // ------------------------------------------------------------------

    fn network_get_response_body(&self, params: &Value) -> Result<Value, ErrorObj> {
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing requestId"))?;
        let body = self
            .response_bodies
            .get(request_id)
            .ok_or_else(|| ErrorObj::server("No data found for resource with given identifier"))?;
        let text = String::from_utf8_lossy(body).into_owned();
        Ok(json!({
            "body": text,
            "base64Encoded": false,
        }))
    }

    fn network_set_extra_headers(&self, params: &Value) -> Result<Value, ErrorObj> {
        let headers = params
            .get("headers")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        self.context.client().set_extra_headers(headers);
        Ok(json!({}))
    }

    fn network_set_cookie(&self, params: &Value) -> Result<Value, ErrorObj> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing name"))?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing value"))?;
        let domain = params
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or("localhost");
        let path = params.get("path").and_then(Value::as_str).unwrap_or("/");
        let secure = params
            .get("secure")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let expires = params
            .get("expires")
            .and_then(Value::as_f64)
            .map(|e| e as i64);
        self.context
            .client()
            .cookies_mut()
            .set_explicit(name, value, domain, path, secure, expires);
        Ok(json!({ "success": true }))
    }

    fn network_get_cookies(&self, params: &Value) -> Result<Value, ErrorObj> {
        let urls = params
            .get("urls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut cookies = Vec::new();
        if urls.is_empty() {
            for c in self.context.client().cookies().all() {
                cookies.push(cookie_json(&c));
            }
        } else {
            for u in urls {
                if let Some(s) = u.as_str()
                    && let Ok(url) = url::Url::parse(s)
                {
                    for (name, value) in self.context.client().cookies().get(&url) {
                        cookies.push(json!({
                            "name": name,
                            "value": value,
                            "domain": url.host_str().unwrap_or(""),
                            "path": "/",
                            "expires": -1,
                            "size": 0,
                            "httpOnly": false,
                            "secure": url.scheme() == "https",
                            "session": true,
                        }));
                    }
                }
            }
        }
        Ok(json!({ "cookies": cookies }))
    }

    fn network_get_all_cookies(&self) -> Result<Value, ErrorObj> {
        let cookies: Vec<Value> = self
            .context
            .client()
            .cookies()
            .all()
            .iter()
            .map(cookie_json)
            .collect();
        Ok(json!({ "cookies": cookies }))
    }

    fn network_delete_cookies(&self, params: &Value) -> Result<Value, ErrorObj> {
        let name = params.get("name").and_then(Value::as_str);
        let domain = params.get("domain").and_then(Value::as_str);
        let path = params.get("path").and_then(Value::as_str);
        self.context
            .client()
            .cookies_mut()
            .delete(name, domain, path);
        Ok(json!({}))
    }

    fn fetch_enable(&mut self, session_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.fetch_enabled = true);
        if let Some(s) = self.sessions.get(session_id) {
            self.fetch_emitters
                .borrow_mut()
                .insert(session_id.to_owned(), s.out.clone());
        }
        let patterns = params
            .get("patterns")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        p.get("urlPattern")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.fetch.set_enabled(true, patterns);
        Ok(json!({}))
    }

    fn fetch_disable(&mut self, session_id: &str) -> Result<Value, ErrorObj> {
        self.set_session_flag(session_id, |s| s.fetch_enabled = false);
        self.fetch_emitters.borrow_mut().remove(session_id);
        let any = self.sessions.values().any(|s| s.fetch_enabled);
        if !any {
            self.fetch.set_enabled(false, Vec::new());
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // DOM
    // ------------------------------------------------------------------

    fn ensure_dom_ids(&self, target_id: &str) -> Result<(), ErrorObj> {
        let page = self.page_ref(target_id)?;
        page.ensure_dom_ids();
        Ok(())
    }

    fn dom_resolve_node(&self, target_id: &str, node_id: i32) -> Result<NodeId, ErrorObj> {
        self.page_ref(target_id)?
            .resolve_node_id(node_id)
            .ok_or_else(|| ErrorObj::server("node with given id does not exist"))
    }

    fn dom_get_document(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        self.ensure_dom_ids(target_id)?;
        let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(1);
        let page = self.page_ref(target_id)?;
        let root = page.dom().document();
        let url = page.url().to_owned();
        Ok(json!({ "root": self.dom_serialize(page, root, depth, true, &url) }))
    }

    fn dom_query_selector(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let (node_id, selector) = self.dom_selector_params(params)?;
        let found = self.dom_query(target_id, node_id, selector, false)?;
        let node_id = match found.into_iter().next() {
            Some(id) => Value::from(self.dom_id(target_id, id)?),
            None => Value::Null,
        };
        Ok(json!({ "nodeId": node_id }))
    }

    fn dom_query_selector_all(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let (node_id, selector) = self.dom_selector_params(params)?;
        let found = self.dom_query(target_id, node_id, selector, true)?;
        let mut ids = Vec::new();
        for id in found {
            ids.push(Value::from(self.dom_id(target_id, id)?));
        }
        Ok(json!({ "nodeIds": ids }))
    }

    fn dom_request_child_nodes(
        &mut self,
        target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(-1);
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        let page = self.page_ref(target_id)?;
        let url = page.url().to_owned();
        let children: Vec<Value> = node
            .children()
            .iter()
            .map(|c| self.dom_serialize(page, *c, depth, true, &url))
            .collect();
        self.emit_gated(
            target_id,
            "DOM.setChildNodes",
            json!({ "parentId": node_id, "nodes": children }),
            |s| s.dom_enabled,
        );
        Ok(json!({}))
    }

    fn dom_get_outer_html(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        let html = self.page_ref(target_id)?.dom().outer_html(node);
        Ok(json!({ "outerHTML": html }))
    }

    fn dom_set_outer_html(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let html = params
            .get("outerHTML")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing outerHTML"))?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        self.page_ref(target_id)?
            .dom()
            .set_outer_html(node, html)
            .map_err(ErrorObj::server)?;
        self.push_preview_snapshot(target_id);
        Ok(json!({}))
    }

    fn dom_get_attributes(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        let attrs = self.page_ref(target_id)?.dom().attributes(node);
        let flat: Vec<Value> = attrs
            .into_iter()
            .flat_map(|(n, v)| vec![n.into(), v.into()])
            .collect();
        Ok(json!({ "attributes": flat }))
    }

    fn dom_set_attribute(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let (node_id, name, value) = self.dom_attribute_params(params)?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        self.page_ref(target_id)?
            .dom()
            .set_attribute(node, name, value);
        self.push_preview_snapshot(target_id);
        Ok(json!({}))
    }

    fn dom_remove_attribute(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let (node_id, name, _) = self.dom_attribute_params(params)?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        self.page_ref(target_id)?.dom().remove_attribute(node, name);
        self.push_preview_snapshot(target_id);
        Ok(json!({}))
    }

    fn dom_set_node_value(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing value"))?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        match node.data() {
            NodeData::Text(_) | NodeData::Comment(_) => {
                self.page_ref(target_id)?
                    .dom()
                    .set_character_data(node, value);
                self.push_preview_snapshot(target_id);
                Ok(json!({}))
            }
            _ => Err(ErrorObj::server("Can only set text and comment nodes")),
        }
    }

    fn dom_remove_node(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        self.page_ref(target_id)?.dom().remove_node(node);
        self.push_preview_snapshot(target_id);
        Ok(json!({}))
    }

    fn dom_focus(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?
            as i32;
        let _ = self.dom_resolve_node(target_id, node_id)?;
        self.page_ref(target_id)?.set_focused_node_id(Some(node_id));
        Ok(json!({}))
    }

    fn dom_resolve_node_obj(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?
            as i32;
        let _ = self.dom_resolve_node(target_id, node_id)?;
        let outcome = self
            .page_ref(target_id)?
            .resolve_node_object(node_id)
            .map_err(ErrorObj::server)?;
        Ok(json!({ "object": remote_object_from_outcome(&outcome, false) }))
    }

    fn dom_describe_node(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        self.ensure_dom_ids(target_id)?;
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(-1);
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        let page = self.page_ref(target_id)?;
        let url = page.url().to_owned();
        Ok(json!({ "node": self.dom_serialize(page, node, depth, true, &url) }))
    }

    fn dom_get_box_model(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let _ = self.dom_resolve_node(
            target_id,
            params.get("nodeId").and_then(Value::as_i64).unwrap_or(0) as i32,
        )?;
        // Approximate: no real layout engine.
        let (w, h, _) = self.viewport;
        let quad = vec![0.0, 0.0, w, 0.0, w, h, 0.0, h];
        Ok(json!({
            "model": {
                "content": quad,
                "padding": quad,
                "border": quad,
                "margin": quad,
                "width": w,
                "height": h,
            }
        }))
    }

    fn dom_get_content_quads(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let model = self.dom_get_box_model(target_id, params)?;
        Ok(json!({ "quads": [model["model"]["content"].clone()] }))
    }

    /// Serialize `node` into a CDP `DOM.Node`.
    fn dom_serialize(
        &self,
        page: &Page,
        node: NodeId,
        depth: i64,
        with_children: bool,
        url: &str,
    ) -> Value {
        let node_id = page.node_id_of(node);
        let parent_id = node.parent().map(|p| page.node_id_of(p));
        let (node_type, node_name, local_name, node_value) = match node.data() {
            NodeData::Document => (9, "#document".to_owned(), String::new(), String::new()),
            NodeData::DocumentType { name, .. } => (10, name.clone(), String::new(), String::new()),
            NodeData::Text(t) => (3, "#text".to_owned(), String::new(), t.clone()),
            NodeData::Comment(c) => (8, "#comment".to_owned(), String::new(), c.clone()),
            NodeData::Element(e) => (
                1,
                e.name().as_ref().to_ascii_uppercase(),
                e.name().as_ref().to_owned(),
                String::new(),
            ),
        };
        let children = node.children();
        let child_node_count = children.len() as i64;

        let mut out = json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": node_type,
            "nodeName": node_name,
            "localName": local_name,
            "nodeValue": node_value,
            "childNodeCount": child_node_count,
        });
        if let Some(pid) = parent_id {
            out["parentId"] = pid.into();
        }
        if let NodeData::Element(e) = node.data() {
            let attrs: Vec<Value> = e
                .attrs
                .iter()
                .flat_map(|a| vec![a.name.as_ref().to_owned().into(), a.value.clone().into()])
                .collect();
            out["attributes"] = Value::Array(attrs);
        }
        if with_children && depth != 0 {
            let kids: Vec<Value> = children
                .iter()
                .map(|c| self.dom_serialize(page, *c, depth - 1, true, url))
                .collect();
            out["children"] = Value::Array(kids);
        }
        if node_type == 9 {
            out["documentURL"] = url.into();
            out["baseURL"] = url.into();
            out["xmlVersion"] = "".into();
            out["compatMode"] = "CSS1Compat".into();
        }
        out
    }

    // --- DOM helpers ----------------------------------------------------

    fn dom_id(&self, target_id: &str, node: NodeId) -> Result<i32, ErrorObj> {
        Ok(self.page_ref(target_id)?.node_id_of(node))
    }

    fn dom_selector_params<'a>(&self, params: &'a Value) -> Result<(i64, &'a str), ErrorObj> {
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?;
        let selector = params
            .get("selector")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing selector"))?;
        Ok((node_id, selector))
    }

    fn dom_attribute_params<'a>(
        &self,
        params: &'a Value,
    ) -> Result<(i64, &'a str, &'a str), ErrorObj> {
        let (node_id, name) = (
            params
                .get("nodeId")
                .and_then(Value::as_i64)
                .ok_or_else(|| ErrorObj::invalid_params("missing nodeId"))?,
            params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ErrorObj::invalid_params("missing name"))?,
        );
        let value = params.get("value").and_then(Value::as_str).unwrap_or("");
        Ok((node_id, name, value))
    }

    fn dom_query(
        &mut self,
        target_id: &str,
        node_id: i64,
        selector: &str,
        all: bool,
    ) -> Result<Vec<NodeId>, ErrorObj> {
        let node = self.dom_resolve_node(target_id, node_id as i32)?;
        let dom = self.page_ref(target_id)?.dom();
        if all {
            dom.query_selector_all_within(node, selector)
                .map_err(ErrorObj::invalid_params)
        } else {
            Ok(dom
                .query_selector_within(node, selector)
                .map_err(ErrorObj::invalid_params)?
                .into_iter()
                .collect())
        }
    }

    // ------------------------------------------------------------------
    // Input / Emulation
    // ------------------------------------------------------------------

    fn input_key(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let text = params.get("text").and_then(Value::as_str).unwrap_or("");
        let key = params.get("key").and_then(Value::as_str).unwrap_or(text);
        let ty = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("keyDown");
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .map(|n| n as i32);
        let _ = self
            .page_ref(target_id)?
            .dispatch_key(node_id, ty, key, text);
        Ok(json!({}))
    }

    fn input_insert_text(&self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let text = params.get("text").and_then(Value::as_str).unwrap_or("");
        self.input_key(target_id, &json!({ "type": "char", "text": text }))
    }

    async fn input_mouse(&mut self, target_id: &str, params: &Value) -> Result<Value, ErrorObj> {
        let ty = params.get("type").and_then(Value::as_str).unwrap_or("");
        let node_id = params
            .get("nodeId")
            .and_then(Value::as_i64)
            .map(|n| n as i32);
        if ty == "mouseMoved" || ty == "mouseover" {
            if let Some(id) = node_id {
                let _ = self.page_ref(target_id)?.dispatch_hover(id);
            }
            return Ok(json!({}));
        }
        if ty == "mousePressed" || ty == "click" {
            let outcome = {
                let page = self.page_ref(target_id)?;
                let id = node_id.or_else(|| page.focused_node_id());
                if let Some(id) = id {
                    page.dispatch_click(id)
                } else {
                    let _ = page.evaluate_js(
                        r#"(function(){
                    var n = document.activeElement || document.body;
                    if (n && n.click) n.click();
                    else if (n) n.dispatchEvent({type:'click',bubbles:true});
                })()"#,
                    );
                    Ok(InteractionOutcome::Applied)
                }
            };
            match outcome {
                Ok(InteractionOutcome::Navigate(url)) => {
                    return self.page_navigate(target_id, &json!({ "url": url })).await;
                }
                Ok(InteractionOutcome::Stale) => {
                    return Err(ErrorObj::named(
                        "stale_node",
                        "preview node is no longer valid",
                    ));
                }
                Ok(InteractionOutcome::Applied) => {}
                Err(e) => return Err(ErrorObj::server(e)),
            }
            self.push_preview_snapshot(target_id);
        }
        Ok(json!({}))
    }

    fn emulation_set_metrics(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let width = params.get("width").and_then(Value::as_f64).unwrap_or(980.0);
        let height = params
            .get("height")
            .and_then(Value::as_f64)
            .unwrap_or(1024.0);
        let dpr = params
            .get("deviceScaleFactor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        self.viewport = (width, height, dpr);
        self.context.set_viewport(width, height, dpr);
        for target in self.targets.values() {
            if let Some(page) = target.page.as_ref() {
                page.set_viewport(width, height, dpr);
            }
        }
        Ok(json!({}))
    }

    fn emulation_clear_metrics(&mut self) -> Result<Value, ErrorObj> {
        self.viewport = (980.0, 1024.0, 1.0);
        self.context.set_viewport(980.0, 1024.0, 1.0);
        Ok(json!({}))
    }

    fn emulation_set_ua(&self, params: &Value) -> Result<Value, ErrorObj> {
        let ua = params
            .get("userAgent")
            .and_then(Value::as_str)
            .unwrap_or("hyperion/0.1");
        self.context.set_user_agent_override(Some(ua.to_owned()));
        self.context.client().set_user_agent(ua);
        for target in self.targets.values() {
            if let Some(page) = target.page.as_ref() {
                page.set_user_agent(ua);
            }
        }
        Ok(json!({}))
    }

    fn target_create_browser_context(&mut self) -> Result<Value, ErrorObj> {
        let id = self.profiles.create_ephemeral("incognito");
        let mut http = HttpClientOptions::default();
        http.allow_private_network = self.config.allow_private_network;
        http.max_body_bytes = self.config.max_body_bytes;
        http.fetch_pause_timeout = self.config.fetch_pause_timeout;
        http.per_origin_connections = self.config.per_origin_connections;
        http.fetch_intercept = Some(self.fetch.clone());
        let ctx = Rc::new(BrowserContext::new(http));
        *ctx.profile_id.borrow_mut() = id.as_str().to_owned();
        self.contexts.insert(id.as_str().to_owned(), ctx);
        Ok(json!({ "browserContextId": id.as_str() }))
    }

    fn target_dispose_browser_context(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        if let Some(id) = params.get("browserContextId").and_then(Value::as_str) {
            let pid = ProfileId(id.to_owned());
            let _ = self.profiles.close(&pid);
            self.contexts.remove(id);
            let doomed: Vec<String> = self
                .targets
                .iter()
                .filter(|(_, t)| t.profile_id == id && t.kind == TargetKind::Page)
                .map(|(tid, _)| tid.clone())
                .collect();
            for tid in doomed {
                self.close_target(&tid);
            }
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // Target / Browser
    // ------------------------------------------------------------------

    fn target_get_targets(&self) -> Value {
        let infos: Vec<Value> = self
            .targets
            .iter()
            .map(|(id, t)| self.target_info(id, t))
            .collect();
        json!({ "targetInfos": infos })
    }

    fn target_get_target_info(
        &self,
        current_target_id: &str,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let target_id = params
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or(current_target_id);
        Ok(json!({
            "targetInfo": self.target_info_details(target_id)?,
        }))
    }

    fn target_info_details(&self, target_id: &str) -> Result<Value, ErrorObj> {
        let target = self
            .targets
            .get(target_id)
            .ok_or_else(|| ErrorObj::server("no such target"))?;
        Ok(json!({
            "targetId": target_id,
            "type": kind_str(target.kind),
            "title": target.title,
            "url": target.url,
            "attached": true,
            "canAccessOpener": false,
            "browserContextId": target.profile_id,
        }))
    }

    async fn target_create(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("about:blank")
            .to_owned();
        let browser_context_id = params.get("browserContextId").and_then(Value::as_str);
        if let Some(id) = browser_context_id
            && !self.contexts.contains_key(id)
        {
            return Err(ErrorObj::invalid_params(format!(
                "unknown browser context: {id}"
            )));
        }
        let id = self.spawn_page_target(&url, browser_context_id).await?;
        Ok(json!({ "targetId": id }))
    }

    fn target_attach(&mut self, conn_session: &str, params: &Value) -> Result<Value, ErrorObj> {
        let target_id = params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing targetId"))?;
        let flatten = params
            .get("flatten")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !self.targets.contains_key(target_id) {
            return Err(ErrorObj::server("no such target"));
        }
        let parent_out = self
            .sessions
            .get(conn_session)
            .map(|s| s.out.clone())
            .ok_or_else(|| ErrorObj::server("session not found"))?;
        let session_id = self.next_session_id();
        self.sessions.insert(
            session_id.clone(),
            Session::new(
                session_id.clone(),
                target_id.to_owned(),
                parent_out.clone(),
                flatten,
            ),
        );
        let info = self.target_info_details(target_id)?;
        let event = protocol::Event {
            method: "Target.attachedToTarget".to_owned(),
            params: json!({
                "sessionId": session_id.clone(),
                "targetInfo": info,
                "waitingForDebugger": false,
            }),
            session_id: None,
        };
        let _ = parent_out.try_send(OutMsg::Text(
            serde_json::to_string(&event).unwrap_or_default(),
        ));
        Ok(json!({ "sessionId": session_id }))
    }

    fn target_detach(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
            self.sessions.remove(sid);
        }
        Ok(json!({}))
    }

    fn target_set_discover_targets(
        &self,
        out: Option<&Sender<OutMsg>>,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        if params
            .get("discover")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && let Some(out) = out
        {
            for (id, t) in &self.targets {
                if t.kind == TargetKind::Page {
                    let event = protocol::Event {
                        method: "Target.targetCreated".to_owned(),
                        params: json!({ "targetInfo": self.target_info(id, t) }),
                        session_id: None,
                    };
                    let _ = out.try_send(OutMsg::Text(
                        serde_json::to_string(&event).unwrap_or_default(),
                    ));
                }
            }
        }
        Ok(json!({}))
    }

    fn target_set_auto_attach(
        &mut self,
        out: Option<&Sender<OutMsg>>,
        params: &Value,
    ) -> Result<Value, ErrorObj> {
        let attach = params
            .get("autoAttach")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let flatten = params
            .get("flatten")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if attach && flatten {
            let Some(out) = out.cloned() else {
                return Ok(json!({}));
            };
            let ids: Vec<String> = self
                .targets
                .iter()
                .filter(|(_, t)| t.kind == TargetKind::Page)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                let info = self.target_info_details(&id)?;
                let session_id = self.next_session_id();
                let event = protocol::Event {
                    method: "Target.attachedToTarget".to_owned(),
                    params: json!({
                        "sessionId": session_id.clone(),
                        "targetInfo": info,
                        "waitingForDebugger": false,
                    }),
                    session_id: None,
                };
                let _ = out.try_send(OutMsg::Text(
                    serde_json::to_string(&event).unwrap_or_default(),
                ));
                self.sessions.insert(
                    session_id.clone(),
                    Session::new(session_id, id, out.clone(), true),
                );
            }
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // Targets / navigation helpers
    // ------------------------------------------------------------------

    async fn new_target(&mut self, url: &str) -> Result<Value, ErrorObj> {
        let id = self.spawn_page_target(url, None).await?;
        let target = self.targets.get(&id).expect("target just inserted");
        Ok(self.target_info(&id, target))
    }

    async fn spawn_page_target(
        &mut self,
        url: &str,
        browser_context_id: Option<&str>,
    ) -> Result<String, ErrorObj> {
        self.admit_active(
            self.active_navigations,
            self.config.max_active_navigations,
            "navigations",
        )?;
        let page_count = self
            .targets
            .values()
            .filter(|t| t.kind == TargetKind::Page)
            .count();
        if page_count >= self.config.max_targets {
            hyperion_metrics::global()
                .admission_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ErrorObj::overloaded("too many targets"));
        }
        let id = format!("page-{}", self.next_target_id);
        self.next_target_id += 1;
        let profile_id = browser_context_id
            .unwrap_or_else(|| self.default_profile.as_str())
            .to_owned();
        self.targets.insert(
            id.clone(),
            Target {
                kind: TargetKind::Page,
                url: url.to_owned(),
                title: String::new(),
                page: None,
                profile_id: profile_id.clone(),
                last_active: Instant::now(),
            },
        );
        hyperion_metrics::global()
            .targets_open
            .store(self.targets.len() as u64, Ordering::Relaxed);
        self.active_navigations += 1;
        hyperion_metrics::global()
            .targets_active
            .store(self.active_navigations as u64, Ordering::Relaxed);
        let context = self
            .contexts
            .get(&profile_id)
            .cloned()
            .ok_or_else(|| ErrorObj::invalid_params("unknown browser context"))?;
        context.client().set_target_id(Some(id.clone()));
        let result = context.navigate(url).await;
        self.active_navigations = self.active_navigations.saturating_sub(1);
        hyperion_metrics::global()
            .targets_active
            .store(self.active_navigations as u64, Ordering::Relaxed);
        if let Ok(page) = result {
            self.install_page(&id, page);
            hyperion_metrics::global()
                .navigations_ok
                .fetch_add(1, Ordering::Relaxed);
        } else {
            hyperion_metrics::global()
                .navigations_err
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(id)
    }

    fn install_page(&mut self, target_id: &str, page: Page) {
        let tx = self.event_tx.clone();
        let tid = target_id.to_owned();
        let page = page;
        // Op 0 is the `console` bridge installed by hyperion-js; route console
        // calls into the event pump so Runtime.consoleAPICalled can fire.
        page.register_op(0, move |arg| {
            let (kind, args) = parse_console_arg(arg);
            let _ = tx.send(ServerEvent::Console {
                target_id: tid.clone(),
                kind,
                args,
            });
            "{}".to_owned()
        });
        if let Some(target) = self.targets.get_mut(target_id) {
            let title = page.title();
            let url = page.url().to_owned();
            target.page = Some(page);
            target.title = title;
            target.url = url;
        }
        self.push_preview_snapshot(target_id);
    }

    fn push_preview_snapshot(&self, target_id: &str) {
        let Some(outs) = self.previews.get(target_id) else {
            return;
        };
        let Some(msg) = self.preview_snapshot_value(target_id) else {
            return;
        };
        let text = serde_json::to_string(&msg).unwrap_or_default();
        for out in outs {
            let _ = out.try_send(OutMsg::Text(text.clone()));
        }
    }

    fn preview_snapshot_value(&self, target_id: &str) -> Option<Value> {
        let target = self.targets.get(target_id)?;
        let page = target.page.as_ref()?;
        let html = preview::snapshot_with_base(&target.url, &page.snapshot_html());
        Some(json!({
            "type": "snapshot",
            "url": target.url,
            "title": target.title,
            "html": html,
            "generation": page.bridge_generation(),
            "profileId": target.profile_id,
        }))
    }

    fn push_preview_console(&self, target_id: &str, kind: &str, args: &[String]) {
        let Some(outs) = self.previews.get(target_id) else {
            return;
        };
        let msg = json!({
            "type": "console",
            "level": kind,
            "text": args.join(" "),
        });
        let text = serde_json::to_string(&msg).unwrap_or_default();
        for out in outs {
            let _ = out.try_send(OutMsg::Text(text.clone()));
        }
    }

    fn push_preview_network(&self, target_id: &str, method: &str, status: u16, url: &str) {
        let Some(outs) = self.previews.get(target_id) else {
            return;
        };
        let msg = json!({
            "type": "network",
            "method": method,
            "status": status,
            "url": url,
        });
        let text = serde_json::to_string(&msg).unwrap_or_default();
        for out in outs {
            let _ = out.try_send(OutMsg::Text(text.clone()));
        }
    }

    async fn handle_preview_command(
        &mut self,
        target_id: &str,
        profile_id: Option<&str>,
        msg: PreviewClientMsg,
    ) -> Result<Value, ErrorObj> {
        let PreviewClientMsg::Input {
            id,
            generation,
            node_id,
            action,
            key,
            text,
            event_type,
        } = msg
        else {
            return Ok(preview::preview_ack(0, 0));
        };
        let (is_page, target_profile) = match self.targets.get(target_id) {
            Some(t) => (t.kind == TargetKind::Page, t.profile_id.clone()),
            None => {
                return Ok(preview::preview_error(
                    Some(id),
                    "target_not_found",
                    "no such target",
                    false,
                ));
            }
        };
        if let Some(pid) = profile_id
            && pid != target_profile
        {
            return Ok(preview::preview_error(
                Some(id),
                "target_profile_mismatch",
                "target does not belong to this profile",
                false,
            ));
        }
        if !is_page {
            return Ok(preview::preview_error(
                Some(id),
                "target_not_found",
                "not a page target",
                false,
            ));
        }
        let page_gen = self
            .page_ref(target_id)
            .ok()
            .map(|p| p.bridge_generation())
            .unwrap_or(0);
        if generation != 0 && generation != page_gen {
            return Ok(preview::preview_error(
                Some(id),
                "stale_document",
                "preview snapshot is stale",
                true,
            ));
        }
        if let Some(t) = self.targets.get_mut(target_id) {
            t.last_active = Instant::now();
        }
        let outcome = {
            let page = match self.page_ref(target_id) {
                Ok(p) => p,
                Err(_) => {
                    return Ok(preview::preview_error(
                        Some(id),
                        "target_not_found",
                        "page not ready",
                        true,
                    ));
                }
            };
            match action.as_str() {
                "hover" | "mousemove" => {
                    let Some(nid) = node_id else {
                        return Ok(preview::preview_error(
                            Some(id),
                            "bad_request",
                            "missing nodeId",
                            false,
                        ));
                    };
                    page.dispatch_hover(nid)
                }
                "focus" => {
                    let Some(nid) = node_id else {
                        return Ok(preview::preview_error(
                            Some(id),
                            "bad_request",
                            "missing nodeId",
                            false,
                        ));
                    };
                    page.dispatch_focus(nid)
                }
                "click" => {
                    let Some(nid) = node_id else {
                        return Ok(preview::preview_error(
                            Some(id),
                            "bad_request",
                            "missing nodeId",
                            false,
                        ));
                    };
                    page.dispatch_click(nid)
                }
                "text" => page.dispatch_text(node_id, text.as_deref().unwrap_or("")),
                "key" => page.dispatch_key(
                    node_id,
                    event_type.as_deref().unwrap_or("keydown"),
                    key.as_deref().unwrap_or(""),
                    text.as_deref().unwrap_or(""),
                ),
                "scroll" => {
                    let Some(nid) = node_id else {
                        return Ok(preview::preview_error(
                            Some(id),
                            "bad_request",
                            "missing nodeId",
                            false,
                        ));
                    };
                    page.dispatch_scroll(nid)
                }
                other => {
                    return Ok(preview::preview_error(
                        Some(id),
                        "bad_request",
                        &format!("unknown action: {other}"),
                        false,
                    ));
                }
            }
        };
        match outcome {
            Ok(InteractionOutcome::Stale) => Ok(preview::preview_error(
                Some(id),
                "stale_node",
                "node is no longer valid",
                true,
            )),
            Ok(InteractionOutcome::Navigate(url)) => {
                if let Err(e) = self.page_navigate(target_id, &json!({ "url": url })).await {
                    return Ok(preview::preview_error(
                        Some(id),
                        "navigate",
                        &e.message,
                        true,
                    ));
                }
                let generation = self
                    .page_ref(target_id)
                    .map(|p| p.bridge_generation())
                    .unwrap_or(0);
                Ok(preview::preview_ack(id, generation))
            }
            Ok(InteractionOutcome::Applied) => {
                self.push_preview_snapshot(target_id);
                let generation = self
                    .page_ref(target_id)
                    .map(|p| p.bridge_generation())
                    .unwrap_or(0);
                Ok(preview::preview_ack(id, generation))
            }
            Err(e) => Ok(preview::preview_error(Some(id), "server", &e, false)),
        }
    }

    fn close_target(&mut self, id: &str) -> Value {
        let existed = self.targets.remove(id).is_some();
        self.sessions.retain(|_, s| s.target_id != id);
        self.previews.remove(id);
        self.store_lifecycle_gauges();
        json!(existed)
    }

    fn target_info(&self, id: &str, target: &Target) -> Value {
        let kind = kind_str(target.kind);
        json!({
            "description": "",
            "devtoolsFrontendUrl": "",
            "id": id,
            "title": target.title,
            "type": kind,
            "url": target.url,
            "webSocketDebuggerUrl": format!("ws://{}/devtools/{}/{}", self.addr, kind, id),
        })
    }

    fn next_session_id(&mut self) -> String {
        let id = format!("session-{}", self.next_session_id);
        self.next_session_id += 1;
        id
    }

    // ------------------------------------------------------------------
    // Version / listing
    // ------------------------------------------------------------------

    fn version(&self) -> Value {
        json!({
            "Browser": "hyperion/0.1",
            "Protocol-Version": "1.3",
            "User-Agent": "hyperion/0.1",
            "V8-Version": hyperion_js::v8_version(),
            "webSocketDebuggerUrl": format!("ws://{}/devtools/browser/{}", self.addr, self.browser_target_id),
        })
    }

    fn list_targets(&self) -> Value {
        let infos: Vec<Value> = self
            .targets
            .iter()
            .filter(|(id, _)| **id != self.browser_target_id)
            .map(|(id, t)| self.target_info(id, t))
            .collect();
        Value::Array(infos)
    }

    // ------------------------------------------------------------------
    // Event pump
    // ------------------------------------------------------------------

    fn pump_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                ServerEvent::Net(ne) => self.dispatch_net(ne),
                ServerEvent::Console {
                    target_id,
                    kind,
                    args,
                } => {
                    self.push_preview_console(&target_id, &kind, &args);
                    let params = json!({
                        "type": kind,
                        "args": args
                            .iter()
                            .map(|a| json!({ "type": "string", "value": a }))
                            .collect::<Vec<_>>(),
                        "executionContextId": 1,
                        "timestamp": now_ts(),
                    });
                    self.emit_gated(&target_id, "Runtime.consoleAPICalled", params, |s| {
                        s.runtime_enabled
                    });
                    // Errors and warnings also surface through the Log domain.
                    if kind == "error" || kind == "warning" || kind == "assert" {
                        let entry = json!({
                            "source": "javascript",
                            "level": if kind == "error" { "error" } else { "warning" },
                            "text": args.join(" "),
                            "timestamp": now_ts(),
                        });
                        self.emit_gated(
                            &target_id,
                            "Log.entryAdded",
                            json!({ "entry": entry }),
                            |s| s.log_enabled,
                        );
                    }
                }
            }
        }
    }

    /// Forward an engine network event using the request's owning target id.
    fn dispatch_net(&mut self, event: NetEvent) {
        let target = match &event {
            NetEvent::Request { target_id, .. }
            | NetEvent::Response { target_id, .. }
            | NetEvent::Finished { target_id, .. }
            | NetEvent::Failed { target_id, .. } => target_id.clone(),
        };
        let Some(target) = target else {
            return;
        };
        let ts = now_ts();
        match event {
            NetEvent::Request {
                id,
                url,
                method,
                headers,
                ..
            } => {
                self.emit_gated(
                    &target,
                    "Network.requestWillBeSent",
                    json!({
                        "requestId": id.to_string(),
                        "loaderId": id.to_string(),
                        "documentURL": url,
                        "request": {
                            "url": url,
                            "method": method,
                            "headers": headers_to_json(&headers),
                            "postData": Value::Null,
                            "hasPostData": false,
                            "initialPriority": "Low",
                            "referrerPolicy": "no-referrer-when-downgrade",
                        },
                        "timestamp": ts,
                        "wallTime": ts,
                        "initiator": { "type": "other" },
                        "redirectHasExtraInfo": false,
                        "type": "Document",
                        "frameId": &target,
                    }),
                    |s| s.network_enabled,
                );
            }
            NetEvent::Response {
                id,
                url,
                status,
                headers,
                ..
            } => {
                self.push_preview_network(&target, "GET", status, &url);
                self.emit_gated(
                    &target,
                    "Network.responseReceived",
                    json!({
                        "requestId": id.to_string(),
                        "loaderId": id.to_string(),
                        "timestamp": ts,
                        "type": "Document",
                        "frameId": &target,
                        "hasExtraInfo": false,
                        "response": {
                            "url": url,
                            "status": status,
                            "statusText": "",
                            "headers": headers_to_json(&headers),
                            "mimeType": "text/html",
                            "connectionReused": false,
                            "connectionId": 1,
                            "encodedDataLength": 0,
                            "securityState": "neutral",
                            "fromDiskCache": false,
                            "fromServiceWorker": false,
                            "fromPrefetchCache": false,
                            "protocol": "http",
                            "timing": { "requestTime": ts },
                        },
                    }),
                    |s| s.network_enabled,
                );
            }
            NetEvent::Finished { id, body, .. } => {
                self.response_bodies.insert(id.to_string(), body.to_vec());
                self.emit_gated(
                    &target,
                    "Network.loadingFinished",
                    json!({
                        "requestId": id.to_string(),
                        "timestamp": ts,
                        "encodedDataLength": body.len(),
                        "shouldReportCorbBlocking": false,
                    }),
                    |s| s.network_enabled,
                );
            }
            NetEvent::Failed { id, error, .. } => {
                self.emit_gated(
                    &target,
                    "Network.loadingFailed",
                    json!({
                        "requestId": id.to_string(),
                        "timestamp": ts,
                        "type": "Document",
                        "errorText": error,
                        "canceled": false,
                    }),
                    |s| s.network_enabled,
                );
            }
        }
    }

    fn begin_shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        hyperion_metrics::global()
            .shutdown
            .store(1, Ordering::Relaxed);
        let _ = self.profiles.flush_all();
        for session in self.sessions.values() {
            let _ = session.out.try_send(OutMsg::Close);
        }
        self.sessions.clear();
        self.previews.clear();
        self.store_lifecycle_gauges();
    }

    fn sweep_idle(&mut self) {
        if self.last_idle_sweep.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_idle_sweep = Instant::now();
        let ttl = self.config.idle_target_ttl;
        let doomed: Vec<String> = self
            .targets
            .iter()
            .filter(|(_, t)| t.kind == TargetKind::Page && t.last_active.elapsed() > ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in doomed {
            self.close_target(&id);
        }
        let _ = self.profiles.heartbeat();
        self.refresh_gauges();
        let idle = self
            .targets
            .values()
            .filter(|t| t.kind == TargetKind::Page)
            .count();
        hyperion_metrics::global()
            .targets_idle
            .store(idle as u64, Ordering::Relaxed);
    }

    fn profile_list(&self, params: &Value) -> Result<Value, ErrorObj> {
        let q = params.get("query").and_then(Value::as_str);
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        let mut rows = self
            .profiles
            .catalog_list(offset, limit.max(1), q)
            .map_err(|e| ErrorObj::server(e.to_string()))?
            .into_iter()
            .map(|p| {
                let local = p
                    .owner_node
                    .as_deref()
                    .map(|o| o == self.config.node_id)
                    .unwrap_or(true);
                json!({
                    "id": p.id.as_str(),
                    "name": p.name,
                    "kind": match p.kind { ProfileKind::Persistent => "persistent", ProfileKind::Ephemeral => "ephemeral" },
                    "owner_node": p.owner_node,
                    "version": p.version,
                    "local": local,
                })
            })
            .collect::<Vec<_>>();
        if offset == 0 {
            for p in self.profiles.list_loaded() {
                if p.kind != ProfileKind::Ephemeral {
                    continue;
                }
                if q.map(|s| p.name.contains(s) || p.id.as_str().contains(s)) == Some(false) {
                    continue;
                }
                if rows.iter().any(|r| r["id"] == p.id.as_str()) {
                    continue;
                }
                rows.push(json!({
                    "id": p.id.as_str(),
                    "name": p.name,
                    "kind": "ephemeral",
                    "owner_node": self.config.node_id,
                    "loaded": true,
                    "local": true,
                }));
            }
        }
        if rows.len() > limit.max(1) {
            rows.truncate(limit.max(1));
        }
        Ok(json!({ "profiles": rows }))
    }

    fn profile_create(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("profile");
        let ephemeral = params
            .get("ephemeral")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let id = if ephemeral {
            self.profiles.create_ephemeral(name)
        } else {
            self.profiles
                .create_persistent(name)
                .map_err(|e| ErrorObj::server(e.to_string()))?
        };
        self.ensure_profile_context(&id);
        Ok(json!({ "profileId": id.as_str() }))
    }

    fn profile_open(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let id = params
            .get("profileId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing profileId"))?;
        let pid = ProfileId(id.to_owned());
        self.profiles
            .open(&pid)
            .map_err(|e| ErrorObj::server(e.to_string()))?;
        self.ensure_profile_context(&pid);
        if let Some(p) = self.profiles.get(&pid) {
            if let Some(ctx) = self.contexts.get(id) {
                ctx.set_local_storage(p.local_storage.clone());
                ctx.client().set_cookies(p.cookies.clone());
            }
        }
        Ok(json!({}))
    }

    fn profile_close(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let id = params
            .get("profileId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing profileId"))?;
        self.sync_profile_from_context(id);
        self.profiles
            .close(&ProfileId(id.to_owned()))
            .map_err(|e| ErrorObj::server(e.to_string()))?;
        Ok(json!({}))
    }

    fn profile_delete(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        let id = params
            .get("profileId")
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorObj::invalid_params("missing profileId"))?;
        self.profiles
            .delete(&ProfileId(id.to_owned()))
            .map_err(|e| ErrorObj::server(e.to_string()))?;
        self.contexts.remove(id);
        Ok(json!({}))
    }

    fn profile_flush(&mut self, params: &Value) -> Result<Value, ErrorObj> {
        if let Some(id) = params.get("profileId").and_then(Value::as_str) {
            self.sync_profile_from_context(id);
            self.profiles
                .flush(&ProfileId(id.to_owned()))
                .map_err(|e| ErrorObj::server(e.to_string()))?;
        } else {
            self.profiles
                .flush_all()
                .map_err(|e| ErrorObj::server(e.to_string()))?;
        }
        Ok(json!({}))
    }

    fn ensure_profile_context(&mut self, id: &ProfileId) {
        if self.contexts.contains_key(id.as_str()) {
            return;
        }
        let mut http = HttpClientOptions::default();
        http.allow_private_network = self.config.allow_private_network;
        http.max_body_bytes = self.config.max_body_bytes;
        http.fetch_intercept = Some(self.fetch.clone());
        let ctx = Rc::new(BrowserContext::new(http));
        *ctx.profile_id.borrow_mut() = id.as_str().to_owned();
        self.contexts.insert(id.as_str().to_owned(), ctx);
    }

    fn sync_profile_from_context(&mut self, id: &str) {
        let pid = ProfileId(id.to_owned());
        let cookies = self.contexts.get(id).map(|c| c.client().cookies().clone());
        let storage = self
            .contexts
            .get(id)
            .map(|c| c.local_storage().borrow().clone());
        if let Some(p) = self.profiles.get_mut(&pid) {
            if let Some(c) = cookies {
                p.cookies = c;
            }
            if let Some(s) = storage {
                p.local_storage = s;
            }
            p.dirty = true;
        }
    }

    async fn dashboard_api(
        &mut self,
        path: &str,
        query: Option<&str>,
        method: &str,
    ) -> Result<Value, ErrorObj> {
        let params = query_map(query);
        let q = params.get("q").cloned();
        if path.ends_with("/targets") && method.eq_ignore_ascii_case("POST") {
            return self.dashboard_targets(&params).await;
        }
        if path.ends_with("/profiles/detail") {
            return self.dashboard_profile_detail(&params);
        }
        if path.ends_with("/profiles") && method.eq_ignore_ascii_case("POST") {
            return match params.get("action").map(String::as_str) {
                Some("create") => self.profile_create(&json!({
                    "name": params.get("name").cloned().unwrap_or_else(|| "profile".into()),
                })),
                Some("open") => {
                    let id = params.get("id").cloned().unwrap_or_default();
                    self.ensure_local_profile(&id)?;
                    Ok(json!({ "ok": true }))
                }
                Some("flush") => self.profile_flush(&json!({
                    "profileId": params.get("id").cloned().unwrap_or_default(),
                })),
                Some("delete") => self.profile_delete(&json!({
                    "profileId": params.get("id").cloned().unwrap_or_default(),
                })),
                _ => Err(ErrorObj::invalid_params("unknown profile action")),
            };
        }
        if path.ends_with("/overview") {
            let pages: Vec<Value> = self
                .targets
                .iter()
                .filter(|(_, t)| t.kind == TargetKind::Page)
                .map(|(id, t)| {
                    json!({
                        "id": id,
                        "url": t.url,
                        "title": t.title,
                        "profileId": t.profile_id,
                    })
                })
                .collect();
            let (nodes, stale) = match self.profiles.catalog_nodes() {
                Ok(n) => (n, false),
                Err(_) => (Vec::new(), true),
            };
            let extra = json!({
                "stale": stale,
                "workers": [{
                    "id": 0,
                    "bind": self.addr.to_string(),
                    "targets": pages.len(),
                    "queue": hyperion_metrics::global().queue_depth.load(Ordering::Relaxed),
                }],
                "nodes": nodes,
                "pages": pages,
            });
            Ok(self.dashboard.overview_json(extra))
        } else if path.ends_with("/profiles") {
            let offset = params
                .get("offset")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let limit = params
                .get("limit")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(100);
            self.profile_list(&json!({ "query": q, "offset": offset, "limit": limit }))
        } else if path.ends_with("/nodes") {
            let (nodes, stale) = match self.profiles.catalog_nodes() {
                Ok(n) => (n, false),
                Err(_) => (Vec::new(), true),
            };
            Ok(json!({
                "workers": [{
                    "id": 0,
                    "bind": self.addr.to_string(),
                    "targets": self.targets.values().filter(|t| t.kind == TargetKind::Page).count(),
                    "queue": hyperion_metrics::global().queue_depth.load(Ordering::Relaxed),
                }],
                "nodes": nodes,
                "nodeId": self.config.node_id,
                "stale": stale,
            }))
        } else if path.ends_with("/events") {
            Ok(json!({ "events": self.dashboard.events() }))
        } else {
            Err(ErrorObj::method_not_found(path))
        }
    }

    fn ensure_local_profile(&mut self, id: &str) -> Result<(), ErrorObj> {
        let pid = ProfileId(id.to_owned());
        if let Ok(Some(rec)) = self.profiles.catalog_get(&pid) {
            if rec.deleted {
                return Err(ErrorObj::named("profile_not_found", "profile deleted"));
            }
            if let Some(owner) = rec.owner_node.as_deref()
                && owner != self.config.node_id
            {
                return Err(ErrorObj::named(
                    "not_local",
                    format!("profile is owned by {owner}"),
                ));
            }
        }
        if self.profiles.get(&pid).is_some() {
            self.ensure_profile_context(&pid);
            return Ok(());
        }
        match self.profiles.open(&pid) {
            Ok(()) => {
                self.ensure_profile_context(&pid);
                Ok(())
            }
            Err(hyperion_profile::ProfileError::LeaseHeld(owner)) => Err(ErrorObj::named(
                "not_local",
                format!("profile is owned by {owner}"),
            )),
            Err(hyperion_profile::ProfileError::NotFound)
            | Err(hyperion_profile::ProfileError::Deleted) => {
                Err(ErrorObj::named("profile_not_found", "unknown profile"))
            }
            Err(e) => Err(ErrorObj::server(e.to_string())),
        }
    }

    fn require_profile_target(&self, profile_id: &str, target_id: &str) -> Result<(), ErrorObj> {
        let t = self
            .targets
            .get(target_id)
            .ok_or_else(|| ErrorObj::named("target_not_found", "no such target"))?;
        if t.kind != TargetKind::Page {
            return Err(ErrorObj::named("target_not_found", "not a page target"));
        }
        if t.profile_id != profile_id {
            return Err(ErrorObj::named(
                "target_profile_mismatch",
                "target does not belong to this profile",
            ));
        }
        Ok(())
    }

    fn dashboard_profile_detail(
        &mut self,
        params: &HashMap<String, String>,
    ) -> Result<Value, ErrorObj> {
        let id = params.get("id").cloned().unwrap_or_default();
        if id.is_empty() {
            return Err(ErrorObj::named("profile_not_found", "missing profile id"));
        }
        self.ensure_local_profile(&id)?;
        let pid = ProfileId(id.clone());
        let loaded = self.profiles.get(&pid);
        let name = loaded.map(|p| p.name.clone()).unwrap_or_default();
        let kind = loaded
            .map(|p| match p.kind {
                ProfileKind::Persistent => "persistent",
                ProfileKind::Ephemeral => "ephemeral",
            })
            .unwrap_or("persistent");
        let targets: Vec<Value> = self
            .targets
            .iter()
            .filter(|(_, t)| t.kind == TargetKind::Page && t.profile_id == id)
            .map(|(tid, t)| {
                json!({
                    "id": tid,
                    "url": t.url,
                    "title": t.title,
                    "profileId": t.profile_id,
                })
            })
            .collect();
        Ok(json!({
            "id": id,
            "name": name,
            "kind": kind,
            "local": true,
            "owner_node": self.config.node_id,
            "targets": targets,
        }))
    }

    async fn dashboard_targets(
        &mut self,
        params: &HashMap<String, String>,
    ) -> Result<Value, ErrorObj> {
        let action = params.get("action").map(String::as_str).unwrap_or("");
        let profile_id = params
            .get("profileId")
            .cloned()
            .or_else(|| params.get("profile_id").cloned())
            .unwrap_or_default();
        if profile_id.is_empty() {
            return Err(ErrorObj::named("profile_not_found", "missing profileId"));
        }
        match action {
            "create" => {
                self.ensure_local_profile(&profile_id)?;
                let url = params
                    .get("url")
                    .cloned()
                    .unwrap_or_else(|| "about:blank".into());
                let id = self.spawn_page_target(&url, Some(&profile_id)).await?;
                let target = self.targets.get(&id);
                Ok(json!({
                    "id": id,
                    "targetId": id,
                    "url": target.map(|t| t.url.clone()).unwrap_or(url),
                    "title": target.map(|t| t.title.clone()).unwrap_or_default(),
                    "profileId": profile_id,
                }))
            }
            "navigate" => {
                let tid = params.get("id").cloned().unwrap_or_default();
                let url = params.get("url").cloned().unwrap_or_default();
                self.require_profile_target(&profile_id, &tid)?;
                self.page_navigate(&tid, &json!({ "url": url })).await?;
                let target = self.targets.get(&tid);
                Ok(json!({
                    "id": tid,
                    "url": target.map(|t| t.url.clone()).unwrap_or(url),
                    "title": target.map(|t| t.title.clone()).unwrap_or_default(),
                    "profileId": profile_id,
                }))
            }
            "close" => {
                let tid = params.get("id").cloned().unwrap_or_default();
                self.require_profile_target(&profile_id, &tid)?;
                Ok(json!({ "closed": self.close_target(&tid) }))
            }
            "activate" => {
                let tid = params.get("id").cloned().unwrap_or_default();
                self.require_profile_target(&profile_id, &tid)?;
                if let Some(t) = self.targets.get_mut(&tid) {
                    t.last_active = Instant::now();
                }
                Ok(json!({ "ok": true }))
            }
            _ => Err(ErrorObj::invalid_params("unknown target action")),
        }
    }

    fn admit_active(&self, current: usize, max: usize, what: &str) -> Result<(), ErrorObj> {
        if current >= max {
            hyperion_metrics::global()
                .admission_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ErrorObj::overloaded(format!("too many {what}")));
        }
        Ok(())
    }

    fn store_lifecycle_gauges(&self) {
        let metrics = hyperion_metrics::global();
        metrics
            .targets_open
            .store(self.targets.len() as u64, Ordering::Relaxed);
        metrics
            .sessions
            .store(self.sessions.len() as u64, Ordering::Relaxed);
        let previews: usize = self.previews.values().map(|v| v.len()).sum();
        metrics.previews.store(previews as u64, Ordering::Relaxed);
        metrics
            .profiles_loaded
            .store(self.profiles.list_loaded().len() as u64, Ordering::Relaxed);
        metrics
            .object_registry
            .store(hyperion_js::live_isolates() as u64, Ordering::Relaxed);
    }

    fn refresh_gauges(&self) {
        self.store_lifecycle_gauges();
        let heap = (hyperion_js::live_isolates() as u64)
            .saturating_mul(self.config.v8_heap_mb as u64)
            .saturating_mul(1024 * 1024);
        hyperion_metrics::refresh_process_gauges(heap);
    }

    /// Send `method` to the sessions attached to `target_id` that pass `gate`.
    fn emit_gated(
        &self,
        target_id: &str,
        method: &str,
        params: Value,
        gate: impl Fn(&Session) -> bool,
    ) {
        for session in self.sessions.values() {
            if session.target_id == target_id && gate(session) {
                let event = protocol::Event {
                    method: method.to_owned(),
                    params: params.clone(),
                    session_id: session.flattened.then(|| session.id.clone()),
                };
                if session
                    .out
                    .try_send(OutMsg::Text(
                        serde_json::to_string(&event).unwrap_or_default(),
                    ))
                    .is_err()
                {
                    hyperion_metrics::global()
                        .queue_drops
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// ----------------------------------------------------------------------
// Free helpers
// ----------------------------------------------------------------------

fn build_profile_manager(config: &Config) -> ProfileManager {
    let key = config
        .encryption_key
        .as_ref()
        .and_then(|b| hyperion_profile::Key::from_slice(b).ok())
        .unwrap_or_else(hyperion_profile::Key::random);
    let store: Box<dyn hyperion_profile::ProfileStore> = match config.profile_backend {
        hyperion_config::ProfileBackend::S3 => {
            match hyperion_profile::S3Store::from_config(&config.s3) {
                Ok(s) => Box::new(s),
                Err(_) => Box::new(hyperion_profile::MemoryStore::new()),
            }
        }
        hyperion_config::ProfileBackend::Local => {
            match hyperion_profile::LocalStore::new(&config.profile_root) {
                Ok(s) => Box::new(s),
                Err(_) => Box::new(hyperion_profile::MemoryStore::new()),
            }
        }
    };
    let memory: Box<dyn hyperion_profile::Catalog> =
        Box::new(hyperion_profile::MemoryCatalog::new());
    let catalog: Box<dyn hyperion_profile::Catalog> =
        if let Some(url) = config.postgres_url.as_deref() {
            match hyperion_profile::PostgresCatalog::connect(url) {
                Ok(pg) => Box::new(pg),
                Err(_) => memory,
            }
        } else {
            memory
        };
    ProfileManager::new(catalog, store, key, config.node_id.clone(), 256)
}

fn query_map(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(q) = query {
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            map.insert(k.into_owned(), v.into_owned());
        }
    }
    map
}

fn is_page_domain(method: &str) -> bool {
    const PAGE_DOMAINS: [&str; 12] = [
        "Runtime.",
        "Page.",
        "DOM.",
        "Network.",
        "Emulation.",
        "Log.",
        "Profiler.",
        "Memory.",
        "Performance.",
        "Debugger.",
        "Input.",
        "Fetch.",
    ];
    PAGE_DOMAINS.iter().any(|d| method.starts_with(d))
}

fn kind_str(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Page => "page",
        TargetKind::Browser => "browser",
    }
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn url_domain(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("")
        .to_owned()
}

/// `scheme://authority` origin for a URL (`http://127.0.0.1:8080/path` → the
/// authority portion keeps host *and* port).
fn url_origin(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("{scheme}://{authority}")
        }
        None => url.to_owned(),
    }
}

fn headers_to_json(headers: &http::HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        map.insert(
            name.as_str().to_owned(),
            value.to_str().unwrap_or_default().to_owned().into(),
        );
    }
    Value::Object(map)
}

/// Build a CDP `RemoteObject` from an [`EvalOutcome`].
fn remote_object_from_outcome(outcome: &EvalOutcome, return_by_value: bool) -> Value {
    if outcome.is_exception {
        return json!({
            "type": "object",
            "subtype": "error",
            "className": "Error",
            "description": outcome.exception.clone().unwrap_or_default(),
        });
    }
    let mut obj = json!({
        "type": outcome.type_name,
        "description": outcome.description.clone().unwrap_or_else(|| outcome.value.clone()),
    });
    if let Some(st) = &outcome.subtype {
        obj["subtype"] = st.clone().into();
    }
    if let Some(cn) = &outcome.class_name {
        obj["className"] = cn.clone().into();
    }
    if let Some(oid) = &outcome.object_id {
        obj["objectId"] = oid.clone().into();
    } else if return_by_value
        || matches!(
            outcome.type_name.as_str(),
            "string" | "number" | "boolean" | "undefined"
        )
    {
        if outcome.type_name == "undefined" {
            // no value
        } else if outcome.type_name == "object" && outcome.subtype.as_deref() == Some("null") {
            obj["value"] = Value::Null;
        } else if outcome.type_name == "string" {
            obj["value"] = Value::String(outcome.value.clone());
        } else if let Ok(v) = serde_json::from_str::<Value>(&outcome.value) {
            obj["value"] = v;
        } else {
            obj["value"] = outcome.value.clone().into();
        }
    } else {
        obj["value"] = outcome.value.clone().into();
    }
    obj
}

fn cookie_json(c: &hyperion_net::cookie::CookieEntry) -> Value {
    json!({
        "name": c.name,
        "value": c.value,
        "domain": c.domain,
        "path": c.path,
        "expires": c.expires.unwrap_or(-1),
        "size": c.name.len() + c.value.len(),
        "httpOnly": false,
        "secure": c.secure,
        "session": c.expires.is_none(),
    })
}

/// Build a `RemoteObject` from the string result of an evaluation. With
/// `returnByValue` we can upgrade strings that happen to be valid JSON to the
/// corresponding typed value (numbers, booleans, null, arrays).
#[allow(dead_code)]
fn remote_object(value: &str, return_by_value: bool) -> Value {
    if return_by_value {
        if value == "undefined" {
            return json!({ "type": "undefined" });
        }
        if let Ok(v) = serde_json::from_str::<Value>(value) {
            let (ty, subtype) = match &v {
                Value::Null => ("object", Some("null")),
                Value::Bool(_) => ("boolean", None),
                Value::Number(_) => ("number", None),
                Value::String(_) => ("string", None),
                Value::Array(_) => ("object", Some("array")),
                Value::Object(_) => ("object", None),
            };
            let mut obj = json!({ "type": ty, "value": v, "description": value });
            if let Some(st) = subtype {
                obj["subtype"] = st.into();
            }
            return obj;
        }
        json!({ "type": "string", "value": value, "description": value })
    } else {
        json!({ "type": "string", "value": value, "description": value })
    }
}

/// Parse the JSON the console op forwards from JS: `{"type":"log","args":[...]}`.
fn parse_console_arg(arg: &str) -> (String, Vec<String>) {
    if let Ok(v) = serde_json::from_str::<Value>(arg) {
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("log")
            .to_owned();
        let args = v
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        (kind, args)
    } else {
        ("log".to_owned(), Vec::new())
    }
}
