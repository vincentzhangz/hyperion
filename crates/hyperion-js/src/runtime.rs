//! The V8 runtime and its synchronous op bridge.
//!
//! The isolate lives on a dedicated worker thread. The main thread drives the
//! runtime through `JsRuntime`, which owns the worker's eval channel and the
//! inbound op-request channel:
//!
//! ```text
//!   main thread                      JS worker thread
//!   -----------                      ----------------
//!   evaluate(src) ── Eval ─────────▶ run script
//!   ...                 ◀─ result ── (may call __hyperion_op)
//!   handle_op(req) ◀──── OpRequest ──┐ (from inside V8 callback)
//!   ────────────── OpResponse ─────▶ resumes callback, returns value to JS
//! ```
//!
//! Ops are synchronous: while a script is blocked inside `__hyperion_op`, the
//! main thread services the op-request channel until the eval completes.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use v8::FunctionCallbackArguments;
use v8::ReturnValue;

use crate::pool::{self, PoolMessage};

/// Payload of an op call from JS: `__hyperion_op(op_id, arg_json)`.
#[derive(Debug)]
pub struct OpRequest {
    pub id: u32,
    pub arg: String,
}

#[derive(Debug)]
pub struct OpResponse {
    pub json: String,
}

/// Structured outcome of an evaluation: the result's string representation
/// plus, when the script threw, the exception text. Feed to
/// `Runtime.evaluate`'s `result` vs `exceptionDetails`.
#[derive(Debug, Clone)]
pub struct EvalOutcome {
    pub value: String,
    pub is_exception: bool,
    pub exception: Option<String>,
    /// CDP-style type: string, number, boolean, undefined, object, function.
    pub type_name: String,
    /// CDP subtype when type is object: null, array, error, node, …
    pub subtype: Option<String>,
    /// Persistent remote-object id when the result is a non-primitive.
    pub object_id: Option<String>,
    /// Class name for objects/functions.
    pub class_name: Option<String>,
    /// Description string shown in DevTools.
    pub description: Option<String>,
}

impl EvalOutcome {
    pub fn primitive(value: String, type_name: &str) -> Self {
        Self {
            description: Some(value.clone()),
            value,
            is_exception: false,
            exception: None,
            type_name: type_name.to_owned(),
            subtype: None,
            object_id: None,
            class_name: None,
        }
    }
}

pub(crate) enum WorkerMessage {
    Eval {
        source: String,
        return_by_value: bool,
        reply_tx: Sender<Result<EvalOutcome, String>>,
    },
    CallFunctionOn {
        object_id: String,
        function_declaration: String,
        arguments_json: Vec<String>,
        return_by_value: bool,
        reply_tx: Sender<Result<EvalOutcome, String>>,
    },
    GetProperties {
        object_id: String,
        own_properties: bool,
        reply_tx: Sender<Result<Vec<PropertyDescriptor>, String>>,
    },
    AwaitPromise {
        object_id: String,
        return_by_value: bool,
        reply_tx: Sender<Result<EvalOutcome, String>>,
    },
    ReleaseObject {
        object_id: String,
        reply_tx: Sender<Result<(), String>>,
    },
    ReleaseObjectGroup {
        object_group: String,
        reply_tx: Sender<Result<(), String>>,
    },
    ResolveNode {
        /// JS expression that returns a node wrapper (`__hyperion_wrapNode(id)`).
        source: String,
        reply_tx: Sender<Result<EvalOutcome, String>>,
    },
    Shutdown,
}

/// A single property returned by [`JsRuntime::get_properties`].
#[derive(Debug, Clone)]
pub struct PropertyDescriptor {
    pub name: String,
    pub value: EvalOutcome,
    pub writable: bool,
    pub configurable: bool,
    pub enumerable: bool,
    pub is_own: bool,
}

/// State reachable from inside a V8 callback: the channels to the main thread.
/// Stored in a thread-local because the isolate is worker-thread-only and the
/// callback must not take a second `&mut` to the isolate (which `get_isolate`
/// would produce) while the callback scope already owns one.
pub(crate) struct OpState {
    pub(crate) op_tx: Sender<OpRequest>,
    pub(crate) op_rx: Receiver<OpResponse>,
}

thread_local! {
    static OP_STATE: std::cell::RefCell<Option<OpState>> = const { std::cell::RefCell::new(None) };
}

/// Handler for an op, running on the main (calling) thread.
///
/// Handlers are intentionally `!Send`: they may capture `Rc` DOM state that
/// lives on the Glommio executor thread. The worker only *requests* ops; the
/// handler always runs on the executor thread inside [`JsRuntime::evaluate`].
type OpHandler = Box<dyn FnMut(&str) -> String>;

/// The JS runtime handle used from the glommio executor thread.
///
/// Isolates live on the shared V8 pool (many per OS thread). This handle
/// only owns the op bridge and a routing id.
pub struct JsRuntime {
    isolate_id: u64,
    pool_tx: Sender<PoolMessage>,
    op_rx: Receiver<OpRequest>,
    op_resp_tx: Sender<OpResponse>,
    op_handlers: HashMap<u32, OpHandler>,
    _platform: v8::SharedPtr<v8::Platform>,
}

impl JsRuntime {
    /// Allocate an isolate on the V8 pool. Panics if V8 fails to initialize.
    pub fn new() -> Self {
        init_v8();

        let isolate_id = pool::V8Pool::alloc_id();
        let pool_tx = pool::global().sender_for(isolate_id);
        let (op_req_tx, op_req_rx) = mpsc::channel();
        let (op_resp_tx, op_resp_rx) = mpsc::channel();
        let (ack_tx, ack_rx) = mpsc::channel();
        pool_tx
            .send(PoolMessage::Create {
                isolate_id,
                op_tx: op_req_tx,
                op_rx: op_resp_rx,
                ack: ack_tx,
            })
            .expect("V8 pool worker closed");
        ack_rx
            .recv()
            .expect("V8 pool worker closed")
            .expect("failed to create isolate");

        Self {
            isolate_id,
            pool_tx,
            op_rx: op_req_rx,
            op_resp_tx,
            op_handlers: HashMap::new(),
            _platform: platform(),
        }
    }

    fn send_work(&self, msg: WorkerMessage) -> Result<(), String> {
        self.pool_tx
            .send(PoolMessage::Work {
                isolate_id: self.isolate_id,
                msg,
            })
            .map_err(|e| format!("V8 pool channel closed: {e}"))
    }

    /// Register an op handler. Handlers run on the calling (main) thread and
    /// return a JSON string delivered to the JS caller.
    pub fn register_op<F>(&mut self, id: u32, handler: F)
    where
        F: FnMut(&str) -> String + 'static,
    {
        self.op_handlers.insert(id, Box::new(handler));
    }

    /// Evaluate `source` in the worker's context and return the result's string
    /// representation. Blocks, servicing any op calls the script makes.
    pub fn evaluate(&mut self, source: &str) -> Result<String, String> {
        self.evaluate_outcome(source).map(|o| o.value)
    }

    /// Like [`evaluate`](Self::evaluate), but also reports whether the script
    /// threw and the exception text.
    pub fn evaluate_outcome(&mut self, source: &str) -> Result<EvalOutcome, String> {
        self.evaluate_outcome_ex(source, false)
    }

    /// Evaluate with optional `returnByValue` semantics for remote objects.
    pub fn evaluate_outcome_ex(
        &mut self,
        source: &str,
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::Eval {
            source: source.to_owned(),
            return_by_value,
            reply_tx,
        })?;
        self.wait_outcome(reply_rx)
    }

    /// Call a function declaration against a remote object.
    pub fn call_function_on(
        &mut self,
        object_id: &str,
        function_declaration: &str,
        arguments_json: &[String],
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::CallFunctionOn {
            object_id: object_id.to_owned(),
            function_declaration: function_declaration.to_owned(),
            arguments_json: arguments_json.to_vec(),
            return_by_value,
            reply_tx,
        })?;
        self.wait_outcome(reply_rx)
    }

    /// Enumerate properties of a remote object.
    pub fn get_properties(
        &mut self,
        object_id: &str,
        own_properties: bool,
    ) -> Result<Vec<PropertyDescriptor>, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::GetProperties {
            object_id: object_id.to_owned(),
            own_properties,
            reply_tx,
        })?;
        self.wait_reply(reply_rx)
    }

    /// Await a Promise remote object (runs microtasks until settled).
    pub fn await_promise(
        &mut self,
        object_id: &str,
        return_by_value: bool,
    ) -> Result<EvalOutcome, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::AwaitPromise {
            object_id: object_id.to_owned(),
            return_by_value,
            reply_tx,
        })?;
        self.wait_outcome(reply_rx)
    }

    /// Drop a remote object by id.
    pub fn release_object(&mut self, object_id: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::ReleaseObject {
            object_id: object_id.to_owned(),
            reply_tx,
        })?;
        self.wait_reply(reply_rx)
    }

    /// Drop every remote object in a group (currently all objects).
    pub fn release_object_group(&mut self, object_group: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::ReleaseObjectGroup {
            object_group: object_group.to_owned(),
            reply_tx,
        })?;
        self.wait_reply(reply_rx)
    }

    /// Evaluate an expression that produces a Node wrapper and retain it as a
    /// remote object (used by `DOM.resolveNode`).
    pub fn resolve_node_object(&mut self, source: &str) -> Result<EvalOutcome, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_work(WorkerMessage::ResolveNode {
            source: source.to_owned(),
            reply_tx,
        })?;
        self.wait_outcome(reply_rx)
    }

    /// Ask the pool to drop this isolate. Pool threads stay alive.
    pub fn shutdown(&mut self) {
        let _ = self.send_work(WorkerMessage::Shutdown);
    }

    fn wait_outcome(
        &mut self,
        reply_rx: Receiver<Result<EvalOutcome, String>>,
    ) -> Result<EvalOutcome, String> {
        self.wait_reply(reply_rx)
    }

    fn wait_reply<T>(&mut self, reply_rx: Receiver<Result<T, String>>) -> Result<T, String> {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        let deadline = std::time::Instant::now() + pool::eval_deadline();
        let mut reply: Option<Result<T, String>> = None;
        while reply.is_none() {
            if std::time::Instant::now() >= deadline {
                return Err("evaluation deadline exceeded".to_owned());
            }
            match reply_rx.try_recv() {
                Ok(r) => reply = Some(r),
                Err(TryRecvError::Empty) => {
                    match self.op_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(req) => self.handle_op(req),
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err("worker disconnected while awaiting op".to_owned());
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => return Err("worker disconnected".to_owned()),
            }
        }
        reply.expect("reply is set above")
    }

    fn handle_op(&mut self, req: OpRequest) {
        let json = match self.op_handlers.get_mut(&req.id) {
            Some(handler) => handler(&req.arg),
            None => format!(r#"{{"error":"unknown op id {}"}}"#, req.id),
        };
        let _ = self.op_resp_tx.send(OpResponse { json });
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn set_op_state(op_tx: Sender<OpRequest>, op_rx: Receiver<OpResponse>) {
    OP_STATE.with(|cell| *cell.borrow_mut() = Some(OpState { op_tx, op_rx }));
}

pub(crate) fn take_op_state() -> Option<OpState> {
    OP_STATE.with(|cell| cell.borrow_mut().take())
}

pub(crate) fn install_prelude(
    scope: &mut v8::ContextScope<'_, '_, v8::HandleScope<'_, v8::Context>>,
) {
    let console_stub = r#"
        (function () {
            var c = {};
            var types = ['log', 'info', 'warn', 'error', 'debug', 'assert'];
            for (var i = 0; i < types.length; i++) {
                (function (t) {
                    c[t] = function () {
                        try {
                            var args = Array.prototype.slice.call(arguments).map(String);
                            __hyperion_op(0, JSON.stringify({ type: t, args: args }));
                        } catch (e) {}
                    };
                })(types[i]);
            }
            globalThis.console = c;
        })();
        'ok'
    "#;
    let _ = eval_in_scope(scope, console_stub, false, &mut ObjectRegistry::new());
}

pub(crate) fn init_v8_platform() {
    init_v8();
}
pub(crate) fn op_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: FunctionCallbackArguments,
    mut rv: ReturnValue,
) {
    let op_id = if args.length() >= 1 {
        args.get(0).to_uint32(scope).map(|u| u.value()).unwrap_or(0)
    } else {
        0
    };
    let arg = if args.length() >= 2 {
        args.get(1)
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let response = OP_STATE.with(|cell| {
        let state = cell.borrow();
        let state = state
            .as_ref()
            .expect("OP_STATE must be set before the isolate runs");

        if state.op_tx.send(OpRequest { id: op_id, arg }).is_err() {
            r#"{"error":"op channel closed"}"#.to_owned()
        } else {
            match state.op_rx.recv() {
                Ok(resp) => resp.json,
                Err(_) => r#"{"error":"op response channel closed"}"#.to_owned(),
            }
        }
    });
    if let Some(s) = v8::String::new(scope, &response) {
        rv.set(s.into());
    }
}

pub(crate) struct ObjectRegistry {
    next_id: u64,
    objects: HashMap<String, v8::Global<v8::Value>>,
    /// object_id -> group name (default "").
    groups: HashMap<String, String>,
    order: VecDeque<String>,
}

impl ObjectRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            objects: HashMap::new(),
            groups: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn next_id(&mut self) -> String {
        let id = format!("{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn insert(&mut self, id: String, global: v8::Global<v8::Value>, group: &str) {
        let max = pool::max_objects();
        while self.objects.len() >= max {
            if let Some(old) = self.order.pop_front() {
                self.release(&old);
            } else {
                break;
            }
        }
        self.objects.insert(id.clone(), global);
        self.groups.insert(id.clone(), group.to_owned());
        self.order.push_back(id);
    }

    fn get<'s>(
        &self,
        scope: &mut v8::ContextScope<'_, 's, v8::HandleScope<'_>>,
        id: &str,
    ) -> Option<v8::Local<'s, v8::Value>> {
        self.objects.get(id).map(|g| v8::Local::new(scope, g))
    }

    pub(crate) fn release(&mut self, id: &str) {
        self.objects.remove(id);
        self.groups.remove(id);
        self.order.retain(|x| x != id);
    }

    pub(crate) fn release_group(&mut self, group: &str) {
        let ids: Vec<String> = self
            .groups
            .iter()
            .filter(|(_, g)| g.as_str() == group || group.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.release(&id);
        }
    }
}

/// Convert a V8 value into an [`EvalOutcome`], optionally retaining non-primitives
/// in `registry`. Implemented as a macro so it accepts both `JsScope` and
/// `TryCatch` (V8 152's PinScope stack does not coerce cleanly across helpers).
macro_rules! value_to_outcome {
    ($scope:expr, $value:expr, $return_by_value:expr, $registry:expr) => {{
        let value = $value;
        let return_by_value = $return_by_value;
        let registry: &mut ObjectRegistry = $registry;

        if value.is_undefined() {
            EvalOutcome {
                value: "undefined".to_owned(),
                is_exception: false,
                exception: None,
                type_name: "undefined".to_owned(),
                subtype: None,
                object_id: None,
                class_name: None,
                description: None,
            }
        } else if value.is_null() {
            EvalOutcome {
                value: "null".to_owned(),
                is_exception: false,
                exception: None,
                type_name: "object".to_owned(),
                subtype: Some("null".to_owned()),
                object_id: None,
                class_name: None,
                description: Some("null".to_owned()),
            }
        } else if value.is_boolean() {
            let b = value.boolean_value($scope);
            EvalOutcome::primitive(if b { "true" } else { "false" }.to_owned(), "boolean")
        } else if value.is_number() {
            let n = value.number_value($scope).unwrap_or(0.0);
            let s = if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            };
            EvalOutcome::primitive(s, "number")
        } else if value.is_string() {
            let s = value
                .to_string($scope)
                .map(|s| s.to_rust_string_lossy($scope))
                .unwrap_or_default();
            EvalOutcome::primitive(s, "string")
        } else {
            let mut subtype = None;
            let mut class_name = Some("Object".to_owned());
            let is_fn = value.is_function();
            if is_fn {
                class_name = Some("Function".to_owned());
            } else if value.is_array() {
                subtype = Some("array".to_owned());
                class_name = Some("Array".to_owned());
            } else if let Some(obj) = value.to_object($scope) {
                let key = v8::String::new($scope, "__nodeId").unwrap();
                if obj.has($scope, key.into()).unwrap_or(false) {
                    subtype = Some("node".to_owned());
                    class_name = Some("HTMLElement".to_owned());
                }
            }

            let desc = value
                .to_string($scope)
                .map(|s| s.to_rust_string_lossy($scope))
                .unwrap_or_else(|| if is_fn { "function" } else { "Object" }.to_owned());

            let type_name = if is_fn {
                "function".to_owned()
            } else {
                "object".to_owned()
            };

            if return_by_value {
                let json_str = if is_fn {
                    desc.clone()
                } else if let Some(s) = v8::json::stringify($scope, value) {
                    s.to_rust_string_lossy($scope)
                } else {
                    desc.clone()
                };
                EvalOutcome {
                    value: json_str,
                    is_exception: false,
                    exception: None,
                    type_name,
                    subtype,
                    object_id: None,
                    class_name,
                    description: Some(desc),
                }
            } else {
                let id = registry.next_id();
                let global = v8::Global::new($scope, value);
                registry.insert(id.clone(), global, "");
                EvalOutcome {
                    value: desc.clone(),
                    is_exception: false,
                    exception: None,
                    type_name,
                    subtype,
                    object_id: Some(id),
                    class_name,
                    description: Some(desc),
                }
            }
        }
    }};
}

pub(crate) fn eval_in_scope(
    scope: &mut v8::ContextScope<'_, '_, v8::HandleScope<'_, v8::Context>>,
    source: &str,
    return_by_value: bool,
    registry: &mut ObjectRegistry,
) -> Result<EvalOutcome, String> {
    let code =
        v8::String::new(scope, source).ok_or_else(|| "failed to allocate source".to_owned())?;
    let script = v8::Script::compile(scope, code, None)
        .ok_or_else(|| format!("failed to compile script: {source}"))?;

    v8::tc_scope!(let tc, scope);
    let result = script.run(tc);
    Ok(match result {
        Some(value) => value_to_outcome!(tc, value, return_by_value, registry),
        None => EvalOutcome {
            value: String::new(),
            is_exception: true,
            exception: tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc)),
            type_name: "object".to_owned(),
            subtype: Some("error".to_owned()),
            object_id: None,
            class_name: Some("Error".to_owned()),
            description: None,
        },
    })
}

pub(crate) fn call_function_on(
    scope: &mut v8::ContextScope<'_, '_, v8::HandleScope<'_, v8::Context>>,
    registry: &mut ObjectRegistry,
    object_id: &str,
    function_declaration: &str,
    arguments_json: &[String],
    return_by_value: bool,
) -> Result<EvalOutcome, String> {
    let this = registry
        .get(scope, object_id)
        .ok_or_else(|| format!("object not found: {object_id}"))?;

    let code = format!("({function_declaration})");
    let code_str =
        v8::String::new(scope, &code).ok_or_else(|| "failed to allocate source".to_owned())?;
    let script = v8::Script::compile(scope, code_str, None)
        .ok_or_else(|| "failed to compile functionDeclaration".to_owned())?;

    v8::tc_scope!(let tc, scope);
    let func_val = script
        .run(tc)
        .ok_or_else(|| "functionDeclaration threw".to_owned())?;
    let func = v8::Local::<v8::Function>::try_from(func_val)
        .map_err(|_| "functionDeclaration did not evaluate to a function".to_owned())?;

    let mut args: Vec<v8::Local<v8::Value>> = Vec::new();
    for arg_json in arguments_json {
        let src = format!("({arg_json})");
        let s = v8::String::new(tc, &src).ok_or_else(|| "arg alloc".to_owned())?;
        let sc = v8::Script::compile(tc, s, None).ok_or_else(|| "arg compile".to_owned())?;
        let v = sc.run(tc).ok_or_else(|| "arg eval".to_owned())?;
        args.push(v);
    }

    let result = func.call(tc, this, &args);
    Ok(match result {
        Some(value) => value_to_outcome!(tc, value, return_by_value, registry),
        None => EvalOutcome {
            value: String::new(),
            is_exception: true,
            exception: tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc)),
            type_name: "object".to_owned(),
            subtype: Some("error".to_owned()),
            object_id: None,
            class_name: Some("Error".to_owned()),
            description: None,
        },
    })
}

pub(crate) fn get_properties(
    scope: &mut v8::ContextScope<'_, '_, v8::HandleScope<'_, v8::Context>>,
    registry: &mut ObjectRegistry,
    object_id: &str,
    own_properties: bool,
) -> Result<Vec<PropertyDescriptor>, String> {
    let value = registry
        .get(scope, object_id)
        .ok_or_else(|| format!("object not found: {object_id}"))?;
    let obj = value
        .to_object(scope)
        .ok_or_else(|| "value is not an object".to_owned())?;

    let names = if own_properties {
        obj.get_own_property_names(scope, Default::default())
    } else {
        obj.get_property_names(scope, Default::default())
    }
    .ok_or_else(|| "failed to enumerate properties".to_owned())?;

    let len = names.length();
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let key = names.get_index(scope, i).unwrap();
        let name = key
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        let prop = obj
            .get(scope, key)
            .unwrap_or_else(|| v8::undefined(scope).into());
        let outcome = value_to_outcome!(scope, prop, false, registry);
        out.push(PropertyDescriptor {
            name,
            value: outcome,
            writable: true,
            configurable: true,
            enumerable: true,
            is_own: true,
        });
    }
    Ok(out)
}

pub(crate) fn await_promise(
    scope: &mut v8::ContextScope<'_, '_, v8::HandleScope<'_, v8::Context>>,
    registry: &mut ObjectRegistry,
    object_id: &str,
    return_by_value: bool,
) -> Result<EvalOutcome, String> {
    let value = registry
        .get(scope, object_id)
        .ok_or_else(|| format!("object not found: {object_id}"))?;

    if !value.is_promise() {
        return Ok(value_to_outcome!(scope, value, return_by_value, registry));
    }

    let promise =
        v8::Local::<v8::Promise>::try_from(value).map_err(|_| "not a promise".to_owned())?;

    for _ in 0..1000 {
        match promise.state() {
            v8::PromiseState::Pending => {
                scope.perform_microtask_checkpoint();
            }
            v8::PromiseState::Fulfilled => {
                let result = promise.result(scope);
                return Ok(value_to_outcome!(scope, result, return_by_value, registry));
            }
            v8::PromiseState::Rejected => {
                let result = promise.result(scope);
                let msg = result
                    .to_string(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "Promise rejected".to_owned());
                return Ok(EvalOutcome {
                    value: String::new(),
                    is_exception: true,
                    exception: Some(msg.clone()),
                    type_name: "object".to_owned(),
                    subtype: Some("error".to_owned()),
                    object_id: None,
                    class_name: Some("Error".to_owned()),
                    description: Some(msg),
                });
            }
        }
    }
    Err("promise did not settle".to_owned())
}

static PLATFORM: OnceLock<v8::SharedPtr<v8::Platform>> = OnceLock::new();

fn init_v8() {
    let _ = platform();
}

pub(crate) fn platform() -> v8::SharedPtr<v8::Platform> {
    PLATFORM
        .get_or_init(|| {
            let platform = v8::new_default_platform(0, false).make_shared();
            v8::V8::initialize_platform(platform.clone());
            v8::V8::initialize();
            platform.into()
        })
        .clone()
}
