//! DOM ops callable from page scripts via `__hyperion_op(1, json)`.
//!
//! The handler runs on the Glommio executor thread and mutates the shared
//! [`Dom`] through an `Rc<RefCell<_>>`. Node identity is a stable integer id
//! shared with the CDP `DOM.*` domain and the live-preview feed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hyperion_dom::{Dom, NodeData, NodeId};
use serde_json::{Value, json};

/// Shared node-id map: arena pointer <-> CDP/JS integer id.
#[derive(Default)]
pub struct NodeIdMap {
    forward: HashMap<NodeId, i32>,
    reverse: HashMap<i32, NodeId>,
    next: i32,
}

impl NodeIdMap {
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
            next: 1,
        }
    }

    pub fn clear(&mut self) {
        self.forward.clear();
        self.reverse.clear();
        self.next = 1;
    }

    /// Ensure every node currently reachable from `root` has an id.
    pub fn ensure_tree(&mut self, root: NodeId) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            self.ensure(id);
            stack.extend(id.children().iter().rev());
        }
    }

    pub fn ensure(&mut self, node: NodeId) -> i32 {
        if let Some(&id) = self.forward.get(&node) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.forward.insert(node, id);
        self.reverse.insert(id, node);
        id
    }

    pub fn get(&self, node: NodeId) -> Option<i32> {
        self.forward.get(&node).copied()
    }

    pub fn resolve(&self, id: i32) -> Option<NodeId> {
        self.reverse.get(&id).copied()
    }

    pub fn forward_map(&self) -> &HashMap<NodeId, i32> {
        &self.forward
    }
}

/// Page-level viewport / UA state mirrored into JS `window` / `navigator`.
#[derive(Clone, Debug)]
pub struct PageEnv {
    pub url: String,
    pub user_agent: String,
    pub width: f64,
    pub height: f64,
    pub dpr: f64,
    pub local_storage: Rc<RefCell<HashMap<String, HashMap<String, String>>>>,
    pub session_storage: Rc<RefCell<HashMap<String, String>>>,
}

impl Default for PageEnv {
    fn default() -> Self {
        Self {
            url: "about:blank".to_owned(),
            user_agent: "hyperion/0.1".to_owned(),
            width: 980.0,
            height: 1024.0,
            dpr: 1.0,
            local_storage: Rc::new(RefCell::new(HashMap::new())),
            session_storage: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// Mutation / focus bookkeeping shared with CDP and the preview feed.
#[derive(Default)]
pub struct DomBridgeState {
    pub focused: Option<i32>,
    /// Incremented on every mutating op so preview can detect changes.
    pub generation: u64,
    /// Event listener registration counts (informational).
    pub listeners: HashMap<(i32, String), u32>,
}

/// Build the op-1 handler that drives the DOM from JS.
pub fn make_dom_op_handler(
    dom: Rc<RefCell<Dom>>,
    ids: Rc<RefCell<NodeIdMap>>,
    env: Rc<RefCell<PageEnv>>,
    state: Rc<RefCell<DomBridgeState>>,
) -> impl FnMut(&str) -> String + 'static {
    move |arg: &str| {
        let Ok(req) = serde_json::from_str::<Value>(arg) else {
            return json!({ "error": "invalid json" }).to_string();
        };
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match dispatch(method, &req, &dom, &ids, &env, &state) {
            Ok(v) => v.to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }
}

fn dispatch(
    method: &str,
    req: &Value,
    dom: &Rc<RefCell<Dom>>,
    ids: &Rc<RefCell<NodeIdMap>>,
    env: &Rc<RefCell<PageEnv>>,
    state: &Rc<RefCell<DomBridgeState>>,
) -> Result<Value, String> {
    match method {
        "documentNode" => {
            let dom = dom.borrow();
            let mut ids = ids.borrow_mut();
            ids.ensure_tree(dom.document());
            Ok(json!({ "nodeId": ids.ensure(dom.document()) }))
        }
        "location" => {
            let env = env.borrow();
            Ok(location_json(&env))
        }
        "viewport" => {
            let env = env.borrow();
            Ok(json!({
                "width": env.width,
                "height": env.height,
                "dpr": env.dpr,
            }))
        }
        "nodeType" => Ok(json!({ "value": node_type(resolve_node(req, ids)?) })),
        "nodeName" => Ok(json!({ "value": node_name(resolve_node(req, ids)?) })),
        "localName" => Ok(json!({ "value": local_name(resolve_node(req, ids)?) })),
        "nodeValue" => Ok(json!({ "value": node_value(resolve_node(req, ids)?) })),
        "setNodeValue" => {
            let node = resolve_node(req, ids)?;
            let value = req
                .get("value")
                .and_then(Value::as_str)
                .ok_or("missing value")?;
            match node.data() {
                NodeData::Text(_) | NodeData::Comment(_) => {
                    dom.borrow().set_character_data(node, value);
                    bump(state);
                    Ok(json!({}))
                }
                _ => Err("Can only set text and comment nodes".to_owned()),
            }
        }
        "textContent" => {
            let node = resolve_node(req, ids)?;
            Ok(json!({ "value": dom.borrow().text_content(node) }))
        }
        "setTextContent" => {
            let node = resolve_node(req, ids)?;
            let value = req
                .get("value")
                .and_then(Value::as_str)
                .ok_or("missing value")?;
            dom.borrow().set_text(node, value.to_owned());
            // New text child needs an id.
            if let Some(child) = node.children().first() {
                ids.borrow_mut().ensure(*child);
            }
            bump(state);
            Ok(json!({}))
        }
        "parentNode" => {
            let node = resolve_node(req, ids)?;
            Ok(json!({ "nodeId": opt_id(node.parent(), ids) }))
        }
        "parentElement" => {
            let node = resolve_node(req, ids)?;
            Ok(json!({ "nodeId": opt_id(node.parent_element(), ids) }))
        }
        "childNodes" => {
            let node = resolve_node(req, ids)?;
            let mut map = ids.borrow_mut();
            let node_ids: Vec<i32> = node.children().iter().map(|c| map.ensure(*c)).collect();
            Ok(json!({ "nodeIds": node_ids }))
        }
        "innerHTML" => {
            let node = resolve_node(req, ids)?;
            Ok(json!({ "value": dom.borrow().inner_html_str(node) }))
        }
        "setInnerHTML" => {
            let node = resolve_node(req, ids)?;
            let html = req
                .get("html")
                .and_then(Value::as_str)
                .ok_or("missing html")?;
            dom.borrow().inner_html(node, html);
            ids.borrow_mut().ensure_tree(node);
            bump(state);
            Ok(json!({}))
        }
        "outerHTML" => {
            let node = resolve_node(req, ids)?;
            Ok(json!({ "value": dom.borrow().outer_html(node) }))
        }
        "setOuterHTML" => {
            let node = resolve_node(req, ids)?;
            let html = req
                .get("html")
                .and_then(Value::as_str)
                .ok_or("missing html")?;
            let parent = node.parent();
            dom.borrow().set_outer_html(node, html)?;
            if let Some(parent) = parent {
                ids.borrow_mut().ensure_tree(parent);
            }
            bump(state);
            Ok(json!({}))
        }
        "getAttribute" => {
            let node = resolve_node(req, ids)?;
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing name")?;
            match dom.borrow().get_attribute(node, name) {
                Some(v) => Ok(json!({ "value": v })),
                None => Ok(json!({ "value": Value::Null })),
            }
        }
        "setAttribute" => {
            let node = resolve_node(req, ids)?;
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing name")?;
            let value = req.get("value").and_then(Value::as_str).unwrap_or("");
            dom.borrow().set_attribute(node, name, value);
            bump(state);
            Ok(json!({}))
        }
        "removeAttribute" => {
            let node = resolve_node(req, ids)?;
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing name")?;
            dom.borrow().remove_attribute(node, name);
            bump(state);
            Ok(json!({}))
        }
        "querySelector" => {
            let node = resolve_node(req, ids)?;
            let selector = req
                .get("selector")
                .and_then(Value::as_str)
                .ok_or("missing selector")?;
            let found = if node_type(node) == 9 {
                dom.borrow().query_selector(selector)?
            } else {
                dom.borrow().query_selector_within(node, selector)?
            };
            Ok(json!({ "nodeId": opt_id(found, ids) }))
        }
        "querySelectorAll" => {
            let node = resolve_node(req, ids)?;
            let selector = req
                .get("selector")
                .and_then(Value::as_str)
                .ok_or("missing selector")?;
            let found = if node_type(node) == 9 {
                dom.borrow().query_selector_all(selector)?
            } else {
                dom.borrow().query_selector_all_within(node, selector)?
            };
            let mut map = ids.borrow_mut();
            let node_ids: Vec<i32> = found.iter().map(|n| map.ensure(*n)).collect();
            Ok(json!({ "nodeIds": node_ids }))
        }
        "createElement" => {
            let tag = req
                .get("tag")
                .and_then(Value::as_str)
                .ok_or("missing tag")?;
            let node = dom.borrow().create_element_by_name(tag);
            let id = ids.borrow_mut().ensure(node);
            bump(state);
            Ok(json!({ "nodeId": id }))
        }
        "createTextNode" => {
            let text = req.get("text").and_then(Value::as_str).unwrap_or("");
            let node = dom.borrow().create_text(text.to_owned());
            let id = ids.borrow_mut().ensure(node);
            bump(state);
            Ok(json!({ "nodeId": id }))
        }
        "appendChild" => {
            let parent = req_node(req, "parentId", ids)?;
            let child = req_node(req, "childId", ids)?;
            // Detach from old parent first if any.
            if child.parent().is_some() {
                dom.borrow().remove_node(child);
            }
            dom.borrow().append_child(parent, child);
            bump(state);
            Ok(json!({}))
        }
        "removeChild" => {
            let child = req_node(req, "childId", ids)?;
            dom.borrow().remove_node(child);
            bump(state);
            Ok(json!({}))
        }
        "removeNode" => {
            let node = resolve_node(req, ids)?;
            dom.borrow().remove_node(node);
            bump(state);
            Ok(json!({}))
        }
        "focus" => {
            let node = resolve_node(req, ids)?;
            let id = ids.borrow_mut().ensure(node);
            state.borrow_mut().focused = Some(id);
            Ok(json!({}))
        }
        "addEventListener" => {
            let node = resolve_node(req, ids)?;
            let id = ids.borrow_mut().ensure(node);
            let ty = req
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("click")
                .to_owned();
            *state.borrow_mut().listeners.entry((id, ty)).or_insert(0) += 1;
            Ok(json!({}))
        }
        "dispatchEvent" => {
            // Events are delivered on the JS side via Node.prototype;
            // this records that a dispatch happened for future Input.* use.
            let _ = resolve_node(req, ids)?;
            Ok(json!({ "ok": true }))
        }
        "storageGet" | "storageSet" | "storageRemove" | "storageClear" | "storageKey"
        | "storageLength" => storage_op(method, req, env),
        _ => Err(format!("unknown dom method: {method}")),
    }
}

fn origin_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .map(|u| {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        })
        .unwrap_or_else(|| url.to_owned())
}

fn storage_op(method: &str, req: &Value, env: &Rc<RefCell<PageEnv>>) -> Result<Value, String> {
    let kind = req.get("kind").and_then(Value::as_str).unwrap_or("local");
    let env = env.borrow();
    let origin = origin_of(&env.url);
    if kind == "session" {
        let mut store = env.session_storage.borrow_mut();
        return match method {
            "storageGet" => {
                let key = req.get("key").and_then(Value::as_str).unwrap_or("");
                Ok(json!({ "value": store.get(key) }))
            }
            "storageSet" => {
                let key = req.get("key").and_then(Value::as_str).unwrap_or("");
                let value = req.get("value").and_then(Value::as_str).unwrap_or("");
                store.insert(key.to_owned(), value.to_owned());
                Ok(json!({}))
            }
            "storageRemove" => {
                let key = req.get("key").and_then(Value::as_str).unwrap_or("");
                store.remove(key);
                Ok(json!({}))
            }
            "storageClear" => {
                store.clear();
                Ok(json!({}))
            }
            "storageKey" => {
                let idx = req.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                Ok(json!({ "value": store.keys().nth(idx) }))
            }
            "storageLength" => Ok(json!({ "value": store.len() })),
            _ => Err("unknown storage method".into()),
        };
    }
    let mut store = env.local_storage.borrow_mut();
    let bucket = store.entry(origin).or_default();
    match method {
        "storageGet" => {
            let key = req.get("key").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "value": bucket.get(key) }))
        }
        "storageSet" => {
            let key = req.get("key").and_then(Value::as_str).unwrap_or("");
            let value = req.get("value").and_then(Value::as_str).unwrap_or("");
            bucket.insert(key.to_owned(), value.to_owned());
            Ok(json!({}))
        }
        "storageRemove" => {
            let key = req.get("key").and_then(Value::as_str).unwrap_or("");
            bucket.remove(key);
            Ok(json!({}))
        }
        "storageClear" => {
            bucket.clear();
            Ok(json!({}))
        }
        "storageKey" => {
            let idx = req.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(json!({ "value": bucket.keys().nth(idx) }))
        }
        "storageLength" => Ok(json!({ "value": bucket.len() })),
        _ => Err("unknown storage method".into()),
    }
}

fn bump(state: &Rc<RefCell<DomBridgeState>>) {
    state.borrow_mut().generation += 1;
}

fn resolve_node(req: &Value, ids: &Rc<RefCell<NodeIdMap>>) -> Result<NodeId, String> {
    req_node(req, "nodeId", ids)
}

fn req_node(req: &Value, key: &str, ids: &Rc<RefCell<NodeIdMap>>) -> Result<NodeId, String> {
    let id = req
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing {key}"))? as i32;
    ids.borrow()
        .resolve(id)
        .ok_or_else(|| format!("node {id} not found"))
}

fn opt_id(node: Option<NodeId>, ids: &Rc<RefCell<NodeIdMap>>) -> Value {
    match node {
        Some(n) => Value::from(ids.borrow_mut().ensure(n)),
        None => Value::Null,
    }
}

fn node_type(node: NodeId) -> i32 {
    match node.data() {
        NodeData::Element(_) => 1,
        NodeData::Text(_) => 3,
        NodeData::Comment(_) => 8,
        NodeData::Document => 9,
        NodeData::DocumentType { .. } => 10,
    }
}

fn node_name(node: NodeId) -> String {
    match node.data() {
        NodeData::Document => "#document".to_owned(),
        NodeData::DocumentType { name, .. } => name.clone(),
        NodeData::Text(_) => "#text".to_owned(),
        NodeData::Comment(_) => "#comment".to_owned(),
        NodeData::Element(e) => e.name().as_ref().to_ascii_uppercase(),
    }
}

fn local_name(node: NodeId) -> String {
    match node.data() {
        NodeData::Element(e) => e.name().as_ref().to_owned(),
        _ => String::new(),
    }
}

fn node_value(node: NodeId) -> String {
    match node.data() {
        NodeData::Text(t) | NodeData::Comment(t) => t.clone(),
        _ => String::new(),
    }
}

fn location_json(env: &PageEnv) -> Value {
    let url = url::Url::parse(&env.url).ok();
    let origin = url
        .as_ref()
        .map(|u| {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        })
        .unwrap_or_default();
    json!({
        "href": env.url,
        "origin": origin,
        "protocol": url.as_ref().map(|u| format!("{}:", u.scheme())).unwrap_or_default(),
        "host": url.as_ref().map(|u| {
            match u.port() {
                Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")),
                None => u.host_str().unwrap_or("").to_owned(),
            }
        }).unwrap_or_default(),
        "hostname": url.as_ref().and_then(|u| u.host_str()).unwrap_or("").to_owned(),
        "port": url.as_ref().and_then(|u| u.port()).map(|p| p.to_string()).unwrap_or_default(),
        "pathname": url.as_ref().map(|u| u.path().to_owned()).unwrap_or_else(|| "/".to_owned()),
        "search": url.as_ref().and_then(|u| u.query()).map(|q| format!("?{q}")).unwrap_or_default(),
        "hash": url.as_ref().and_then(|u| u.fragment()).map(|f| format!("#{f}")).unwrap_or_default(),
        "userAgent": env.user_agent,
    })
}
