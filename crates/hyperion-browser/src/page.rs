//! A single loaded document: DOM + live JS runtime.

use std::cell::RefCell;
use std::rc::Rc;

use hyperion_dom::{Dom, NodeData, NodeId};
use hyperion_js::{EvalOutcome, JsRuntime, PropertyDescriptor};

use crate::domjs::{DomBridgeState, NodeIdMap, PageEnv, make_dom_op_handler};

/// Result of a node-addressed preview/CDP interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionOutcome {
    Applied,
    Stale,
    Navigate(String),
}

/// Op id for the multiplexed DOM bridge (`document` / `Element` methods).
pub const DOM_OP_ID: u32 = 1;

/// A loaded page. The DOM is parsed from the fetched HTML; the JS runtime is a
/// worker-thread V8 isolate that persists across [`evaluate_js`] calls and
/// already ran the page's inline `<script>` blocks during navigation.
///
/// [`evaluate_js`]: Page::evaluate_js
pub struct Page {
    url: String,
    status: u16,
    dom: Rc<RefCell<Dom>>,
    node_ids: Rc<RefCell<NodeIdMap>>,
    env: Rc<RefCell<PageEnv>>,
    bridge: Rc<RefCell<DomBridgeState>>,
    js: RefCell<JsRuntime>,
    /// Scripts to run before inline scripts on this document (and retained for
    /// CDP `Page.addScriptToEvaluateOnNewDocument` across navigations at the
    /// context level — per-page copy lives here after apply).
    init_scripts: Vec<String>,
}

impl Page {
    pub fn new(url: String, status: u16, dom: Dom) -> Self {
        let env = PageEnv {
            url: url.clone(),
            ..PageEnv::default()
        };
        let mut page = Self {
            url,
            status,
            dom: Rc::new(RefCell::new(dom)),
            node_ids: Rc::new(RefCell::new(NodeIdMap::new())),
            env: Rc::new(RefCell::new(env)),
            bridge: Rc::new(RefCell::new(DomBridgeState::default())),
            js: RefCell::new(JsRuntime::new()),
            init_scripts: Vec::new(),
        };
        page.install_dom_bridge();
        page
    }

    pub fn attach_local_storage(
        &self,
        store: Rc<
            RefCell<std::collections::HashMap<String, std::collections::HashMap<String, String>>>,
        >,
    ) {
        self.env.borrow_mut().local_storage = store;
    }

    /// Wire op 0 (console is already in the isolate) is left to the CDP layer;
    /// here we register the DOM op and install the JS prelude.
    fn install_dom_bridge(&mut self) {
        {
            let mut ids = self.node_ids.borrow_mut();
            ids.clear();
            ids.ensure_tree(self.dom.borrow().document());
        }
        let handler = make_dom_op_handler(
            self.dom.clone(),
            self.node_ids.clone(),
            self.env.clone(),
            self.bridge.clone(),
        );
        let mut js = self.js.borrow_mut();
        js.register_op(DOM_OP_ID, handler);
        let _ = js.evaluate(hyperion_js::DOM_PRELUDE);
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn dom(&self) -> std::cell::Ref<'_, Dom> {
        self.dom.borrow()
    }

    pub fn dom_mut(&self) -> std::cell::RefMut<'_, Dom> {
        self.dom.borrow_mut()
    }

    pub fn node_ids(&self) -> std::cell::Ref<'_, NodeIdMap> {
        self.node_ids.borrow()
    }

    pub fn node_ids_mut(&self) -> std::cell::RefMut<'_, NodeIdMap> {
        self.node_ids.borrow_mut()
    }

    pub fn bridge_generation(&self) -> u64 {
        self.bridge.borrow().generation
    }

    pub fn focused_node_id(&self) -> Option<i32> {
        self.bridge.borrow().focused
    }

    pub fn set_focused_node_id(&self, id: Option<i32>) {
        self.bridge.borrow_mut().focused = id;
    }

    pub fn env(&self) -> std::cell::Ref<'_, PageEnv> {
        self.env.borrow()
    }

    pub fn set_viewport(&self, width: f64, height: f64, dpr: f64) {
        {
            let mut env = self.env.borrow_mut();
            env.width = width;
            env.height = height;
            env.dpr = dpr;
        }
        let _ = self.evaluate_js(&format!(
            "window.innerWidth={width};window.innerHeight={height};window.devicePixelRatio={dpr};"
        ));
    }

    pub fn set_user_agent(&self, ua: &str) {
        self.env.borrow_mut().user_agent = ua.to_owned();
        let _ = self.evaluate_js(&format!(
            "window.navigator.userAgent={};",
            serde_json::to_string(ua).unwrap_or_else(|_| "\"hyperion/0.1\"".to_owned())
        ));
    }

    /// Ensure every reachable node has a stable integer id.
    pub fn ensure_dom_ids(&self) {
        let doc = self.dom.borrow().document();
        self.node_ids.borrow_mut().ensure_tree(doc);
    }

    pub fn resolve_node_id(&self, id: i32) -> Option<NodeId> {
        self.node_ids.borrow().resolve(id)
    }

    pub fn node_id_of(&self, node: NodeId) -> i32 {
        self.node_ids.borrow_mut().ensure(node)
    }

    /// Evaluate `source` in the page's JS context and return the result's
    /// string representation.
    pub fn evaluate_js(&self, source: &str) -> Result<String, String> {
        self.with_js_mut(|js| js.evaluate(source))
    }

    /// Evaluate `source` and report whether it threw and the exception text.
    pub fn evaluate_outcome(&self, source: &str) -> Result<EvalOutcome, String> {
        self.with_js_mut(|js| js.evaluate_outcome(source))
    }

    pub fn evaluate_outcome_ex(
        &self,
        source: &str,
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        self.with_js_mut(|js| js.evaluate_outcome_ex(source, return_by_value))
    }

    pub fn call_function_on(
        &self,
        object_id: &str,
        function_declaration: &str,
        arguments_json: &[String],
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        self.with_js_mut(|js| {
            js.call_function_on(
                object_id,
                function_declaration,
                arguments_json,
                return_by_value,
            )
        })
    }

    pub fn get_properties(
        &self,
        object_id: &str,
        own_properties: bool,
    ) -> Result<Vec<PropertyDescriptor>, String> {
        self.with_js_mut(|js| js.get_properties(object_id, own_properties))
    }

    pub fn await_promise(
        &self,
        object_id: &str,
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        self.with_js_mut(|js| js.await_promise(object_id, return_by_value))
    }

    pub fn release_object(&self, object_id: &str) -> Result<(), String> {
        self.with_js_mut(|js| js.release_object(object_id))
    }

    pub fn release_object_group(&self, group: &str) -> Result<(), String> {
        self.with_js_mut(|js| js.release_object_group(group))
    }

    pub fn resolve_node_object(&self, node_id: i32) -> Result<EvalOutcome, String> {
        let source = format!("__hyperion_wrapNode({node_id})");
        self.with_js_mut(|js| js.resolve_node_object(&source))
    }

    /// Register a synchronous op callable from page scripts.
    pub fn register_op<F>(&self, id: u32, handler: F)
    where
        F: FnMut(&str) -> String + 'static,
    {
        self.with_js_mut(|js| {
            js.register_op(id, handler);
        });
    }

    /// Scripts evaluated before inline `<script>` blocks.
    pub fn set_init_scripts(&mut self, scripts: Vec<String>) {
        self.init_scripts = scripts;
    }

    pub fn init_scripts(&self) -> &[String] {
        &self.init_scripts
    }

    /// The `<title>` text, if present.
    pub fn title(&self) -> String {
        self.dom
            .borrow()
            .query_selector("title")
            .ok()
            .flatten()
            .map(|t| self.element_text(t))
            .unwrap_or_default()
    }

    /// All text in the document, in document order.
    pub fn text_content(&self) -> String {
        self.dom.borrow().text_content(self.dom.borrow().document())
    }

    /// Text content of a single element (descendant text, document order).
    pub fn element_text(&self, id: NodeId) -> String {
        self.dom.borrow().text_content(id)
    }

    /// Snapshot HTML for the live preview (document outerHTML with stable node ids).
    pub fn snapshot_html(&self) -> String {
        self.ensure_dom_ids();
        let dom = self.dom.borrow();
        let ids = self.node_ids.borrow();
        dom.outer_html_annotated(dom.document(), |n| ids.get(n))
    }

    /// Dispatch mouseover/enter/move to `node_id`, leaving the previous hover target.
    pub fn dispatch_hover(&self, node_id: i32) -> Result<InteractionOutcome, String> {
        if self.resolve_node_id(node_id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        let prev = self.bridge.borrow().hovered;
        if prev == Some(node_id) {
            let _ = self.eval_flag(&format!(
                "(function(){{var n=__hyperion_wrapNode({node_id});if(!n)return 'stale';n.dispatchEvent({{type:'mousemove',bubbles:true}});return 'ok';}})()"
            ))?;
            return Ok(InteractionOutcome::Applied);
        }
        if let Some(old) = prev {
            let _ = self.evaluate_js(&format!(
                "(function(){{var n=__hyperion_wrapNode({old});if(n){{n.dispatchEvent({{type:'mouseout',bubbles:true}});n.dispatchEvent({{type:'mouseleave',bubbles:false}});}}}})()"
            ));
        }
        self.bridge.borrow_mut().hovered = Some(node_id);
        match self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({node_id});if(!n)return 'stale';n.dispatchEvent({{type:'mouseover',bubbles:true}});n.dispatchEvent({{type:'mouseenter',bubbles:false}});n.dispatchEvent({{type:'mousemove',bubbles:true}});return 'ok';}})()"
        ))? {
            InteractionOutcome::Stale => Ok(InteractionOutcome::Stale),
            other => Ok(other),
        }
    }

    /// Focus `node_id`, firing blur on the previous focused node.
    pub fn dispatch_focus(&self, node_id: i32) -> Result<InteractionOutcome, String> {
        if self.resolve_node_id(node_id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        let prev = self.focused_node_id();
        if prev == Some(node_id) {
            return Ok(InteractionOutcome::Applied);
        }
        if let Some(old) = prev {
            let _ = self.evaluate_js(&format!(
                "(function(){{var n=__hyperion_wrapNode({old});if(n){{n.dispatchEvent({{type:'blur',bubbles:false}});n.dispatchEvent({{type:'focusout',bubbles:true}});}}}})()"
            ));
        }
        self.set_focused_node_id(Some(node_id));
        self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({node_id});if(!n)return 'stale';n.focus();n.dispatchEvent({{type:'focus',bubbles:false}});n.dispatchEvent({{type:'focusin',bubbles:true}});return 'ok';}})()"
        ))
    }

    /// Click `node_id`. Anchor default actions return [`InteractionOutcome::Navigate`].
    pub fn dispatch_click(&self, node_id: i32) -> Result<InteractionOutcome, String> {
        if self.resolve_node_id(node_id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        let _ = self.dispatch_hover(node_id)?;
        let _ = self.dispatch_focus(node_id)?;
        let result = self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({node_id});if(!n)return 'stale';n.dispatchEvent({{type:'mousedown',bubbles:true}});n.dispatchEvent({{type:'mouseup',bubbles:true}});if(n.click)n.click();else n.dispatchEvent({{type:'click',bubbles:true}});return 'ok';}})()"
        ))?;
        if result == InteractionOutcome::Stale {
            return Ok(result);
        }
        if let Some(url) = self.anchor_navigation_url(node_id) {
            return Ok(InteractionOutcome::Navigate(url));
        }
        Ok(InteractionOutcome::Applied)
    }

    /// Insert `text` into the focused (or specified) node and fire input/change.
    pub fn dispatch_text(
        &self,
        node_id: Option<i32>,
        text: &str,
    ) -> Result<InteractionOutcome, String> {
        let Some(id) = node_id.or_else(|| self.focused_node_id()) else {
            return Ok(InteractionOutcome::Applied);
        };
        if self.resolve_node_id(id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        let _ = self.dispatch_focus(id)?;
        let text_js = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({id});if(!n)return 'stale';n.value=(n.value||'')+{text_js};n.dispatchEvent({{type:'input',bubbles:true}});n.dispatchEvent({{type:'change',bubbles:true}});return 'ok';}})()"
        ))
    }

    /// Dispatch a keyboard event to the focused (or specified) node.
    pub fn dispatch_key(
        &self,
        node_id: Option<i32>,
        event_type: &str,
        key: &str,
        text: &str,
    ) -> Result<InteractionOutcome, String> {
        let Some(id) = node_id.or_else(|| self.focused_node_id()) else {
            return Ok(InteractionOutcome::Applied);
        };
        if self.resolve_node_id(id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        let ty = serde_json::to_string(event_type).unwrap_or_else(|_| "\"keydown\"".into());
        let key_js = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
        let _ = self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({id});if(!n)return 'stale';n.dispatchEvent({{type:{ty},key:{key_js},bubbles:true}});return 'ok';}})()"
        ))?;
        if !text.is_empty()
            && (event_type == "keyDown"
                || event_type == "keydown"
                || event_type == "rawKeyDown"
                || event_type == "char")
        {
            return self.dispatch_text(Some(id), text);
        }
        Ok(InteractionOutcome::Applied)
    }

    /// Fire a scroll event on `node_id`. Visual scrolling stays in the operator iframe.
    pub fn dispatch_scroll(&self, node_id: i32) -> Result<InteractionOutcome, String> {
        if self.resolve_node_id(node_id).is_none() {
            return Ok(InteractionOutcome::Stale);
        }
        self.eval_flag(&format!(
            "(function(){{var n=__hyperion_wrapNode({node_id});if(!n)return 'stale';n.dispatchEvent({{type:'scroll',bubbles:true}});return 'ok';}})()"
        ))
    }

    fn eval_flag(&self, source: &str) -> Result<InteractionOutcome, String> {
        let result = self.evaluate_js(source)?;
        let flag = result.trim().trim_matches('"');
        if flag == "stale" {
            Ok(InteractionOutcome::Stale)
        } else {
            Ok(InteractionOutcome::Applied)
        }
    }

    fn anchor_navigation_url(&self, node_id: i32) -> Option<String> {
        let mut cur = self.resolve_node_id(node_id);
        while let Some(node) = cur {
            if let NodeData::Element(e) = node.data()
                && e.name().as_ref() == "a"
            {
                let href = self.dom().get_attribute(node, "href")?;
                if href.is_empty() || href.starts_with("javascript:") || href.starts_with('#') {
                    return None;
                }
                return resolve_href(self.url(), &href);
            }
            cur = node.parent();
        }
        None
    }

    /// Evaluate init scripts then all inline `<script>` blocks in document order.
    pub(crate) fn run_inline_scripts(&self) {
        for script in &self.init_scripts {
            let _ = self.evaluate_js(script);
        }
        let scripts = {
            let dom = self.dom.borrow();
            match dom.query_selector_all("script") {
                Ok(s) => s,
                Err(_) => return,
            }
        };
        for id in scripts {
            // Skip external scripts (src=) for now — Phase 5 may fetch them.
            let has_src = self
                .dom
                .borrow()
                .get_attribute(id, "src")
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if has_src {
                continue;
            }
            let code = self.element_text(id);
            if !code.is_empty() {
                let _ = self.evaluate_js(&code);
            }
        }
    }

    fn with_js_mut<R>(&self, f: impl FnOnce(&mut JsRuntime) -> R) -> R {
        f(&mut self.js.borrow_mut())
    }
}

fn resolve_href(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") || href == "about:blank" {
        return Some(href.to_owned());
    }
    url::Url::parse(base)
        .ok()
        .and_then(|u| u.join(href).ok())
        .map(|u| u.to_string())
}
