//! Bounded V8 worker pool: N OS threads, many isolates per thread.
//!
//! Isolates never move between threads. Each [`crate::JsRuntime`] is hashed
//! onto one worker and multiplexed with other runtimes on that thread.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::runtime::{
    ObjectRegistry, OpRequest, OpResponse, WorkerMessage, await_promise, call_function_on,
    eval_in_scope, get_properties, install_prelude, op_callback, set_op_state, take_op_state,
};

static POOL: OnceLock<V8Pool> = OnceLock::new();
static NEXT_ISOLATE: AtomicU64 = AtomicU64::new(1);
static LIVE_ISOLATES: AtomicUsize = AtomicUsize::new(0);
static MAX_OBJECTS: AtomicUsize = AtomicUsize::new(10_000);
static EVAL_DEADLINE_MS: AtomicU64 = AtomicU64::new(10_000);
static HEAP_MB: AtomicUsize = AtomicUsize::new(128);

pub fn configure(pool_size: usize, heap_mb: usize, max_objects: usize, eval_deadline: Duration) {
    HEAP_MB.store(heap_mb.max(16), Ordering::Relaxed);
    MAX_OBJECTS.store(max_objects.max(64), Ordering::Relaxed);
    EVAL_DEADLINE_MS.store(eval_deadline.as_millis() as u64, Ordering::Relaxed);
    let _ = pool_size;
    let _ = global();
}

pub fn max_objects() -> usize {
    MAX_OBJECTS.load(Ordering::Relaxed)
}

pub fn live_isolates() -> usize {
    LIVE_ISOLATES.load(Ordering::Relaxed)
}

pub fn eval_deadline() -> Duration {
    Duration::from_millis(EVAL_DEADLINE_MS.load(Ordering::Relaxed).max(50))
}

pub(crate) fn global() -> &'static V8Pool {
    POOL.get_or_init(|| {
        let n = std::env::var("HYPERION_V8_POOL_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get().clamp(1, 16))
                    .unwrap_or(4)
            });
        V8Pool::new(n.max(1))
    })
}

pub(crate) struct V8Pool {
    workers: Vec<Sender<PoolMessage>>,
}

pub(crate) enum PoolMessage {
    Create {
        isolate_id: u64,
        op_tx: Sender<OpRequest>,
        op_rx: Receiver<OpResponse>,
        ack: Sender<Result<(), String>>,
    },
    Work {
        isolate_id: u64,
        msg: WorkerMessage,
    },
}

impl V8Pool {
    fn new(n: usize) -> Self {
        crate::runtime::init_v8_platform();
        let mut workers = Vec::with_capacity(n);
        for i in 0..n {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name(format!("hyperion-v8-{i}"))
                .spawn(move || pool_worker(rx))
                .expect("failed to spawn V8 pool worker");
            workers.push(tx);
        }
        Self { workers }
    }

    pub fn alloc_id() -> u64 {
        NEXT_ISOLATE.fetch_add(1, Ordering::Relaxed)
    }

    pub fn sender_for(&self, isolate_id: u64) -> Sender<PoolMessage> {
        let idx = (isolate_id as usize) % self.workers.len();
        self.workers[idx].clone()
    }
}

struct Slot {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    registry: ObjectRegistry,
    op_tx: Sender<OpRequest>,
    op_rx: Option<Receiver<OpResponse>>,
}

/// rusty_v8 enters an isolate on `Isolate::new` and requires `GetCurrent()` to
/// match before `ContextScope` or `Drop`. Park isolates when idle so many can
/// share one OS thread, then re-enter for work and disposal.
struct IsolateEnter {
    isolate: *const v8::OwnedIsolate,
}

impl IsolateEnter {
    fn new(isolate: &v8::OwnedIsolate) -> Self {
        // SAFETY: this isolate is parked (exited after Create or the previous
        // dispatch). Re-entering is allowed; we exit in Drop after all scopes.
        unsafe { isolate.enter() };
        Self { isolate }
    }
}

impl Drop for IsolateEnter {
    fn drop(&mut self) {
        // SAFETY: `enter` paired this isolate as current; HandleScopes are gone.
        unsafe { (*self.isolate).exit() };
    }
}

fn drop_slot(slot: Slot) {
    // Globals in `registry`/`context` drop before `OwnedIsolate`. Enter first so
    // those destructors and isolate `Drop` see this isolate as current.
    // SAFETY: isolate is parked; Drop of OwnedIsolate will exit and dispose.
    unsafe { slot.isolate.enter() };
    drop(slot);
    LIVE_ISOLATES.fetch_sub(1, Ordering::Relaxed);
}

fn pool_worker(rx: Receiver<PoolMessage>) {
    let mut slots: HashMap<u64, Slot> = HashMap::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            PoolMessage::Create {
                isolate_id,
                op_tx,
                op_rx,
                ack,
            } => {
                let result = create_slot(op_tx, op_rx).map(|slot| {
                    LIVE_ISOLATES.fetch_add(1, Ordering::Relaxed);
                    slots.insert(isolate_id, slot);
                });
                let _ = ack.send(result);
            }
            PoolMessage::Work { isolate_id, msg } => {
                if matches!(msg, WorkerMessage::Shutdown) {
                    if let Some(slot) = slots.remove(&isolate_id) {
                        drop_slot(slot);
                    }
                    continue;
                }
                let Some(slot) = slots.get_mut(&isolate_id) else {
                    reply_missing(&msg);
                    continue;
                };
                dispatch_on_slot(slot, msg);
            }
        }
    }
    let leftover: Vec<Slot> = slots.drain().map(|(_, slot)| slot).collect();
    for slot in leftover {
        drop_slot(slot);
    }
}

fn create_slot(op_tx: Sender<OpRequest>, op_rx: Receiver<OpResponse>) -> Result<Slot, String> {
    let heap = HEAP_MB.load(Ordering::Relaxed) * 1024 * 1024;
    let mut params = v8::CreateParams::default();
    params = params.heap_limits(1024 * 1024, heap);
    let mut isolate = v8::Isolate::new(params);
    let context_global = {
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let global_ctx = v8::Global::new(handle_scope, context);
        {
            let mut context_scope = v8::ContextScope::new(handle_scope, context);
            let scope = &mut context_scope;
            let template = v8::FunctionTemplate::new(scope, op_callback);
            let func = template.get_function(scope).ok_or("op function")?;
            let key = v8::String::new(scope, "__hyperion_op")
                .ok_or("op key")?
                .into();
            context.global(scope).set(scope, key, func.into());
            // Prelude only *defines* console; it does not call into ops.
            install_prelude(scope);
        }
        global_ctx
    };
    // Park so the next Create on this thread can enter a different isolate.
    // SAFETY: HandleScopes from setup have been dropped; isolate is current.
    unsafe { isolate.exit() };
    Ok(Slot {
        isolate,
        context: context_global,
        registry: ObjectRegistry::new(),
        op_tx,
        op_rx: Some(op_rx),
    })
}

fn dispatch_on_slot(slot: &mut Slot, msg: WorkerMessage) {
    let _entered = IsolateEnter::new(&slot.isolate);
    let op_rx = slot.op_rx.take().expect("op_rx");
    set_op_state(slot.op_tx.clone(), op_rx);
    {
        v8::scope!(let handle_scope, &mut slot.isolate);
        let context = v8::Local::new(handle_scope, &slot.context);
        let mut context_scope = v8::ContextScope::new(handle_scope, context);
        let scope = &mut context_scope;
        match msg {
            WorkerMessage::Eval {
                source,
                return_by_value,
                reply_tx,
            } => {
                let result = eval_in_scope(scope, &source, return_by_value, &mut slot.registry);
                let _ = reply_tx.send(result);
            }
            WorkerMessage::CallFunctionOn {
                object_id,
                function_declaration,
                arguments_json,
                return_by_value,
                reply_tx,
            } => {
                let result = call_function_on(
                    scope,
                    &mut slot.registry,
                    &object_id,
                    &function_declaration,
                    &arguments_json,
                    return_by_value,
                );
                let _ = reply_tx.send(result);
            }
            WorkerMessage::GetProperties {
                object_id,
                own_properties,
                reply_tx,
            } => {
                let result = get_properties(scope, &mut slot.registry, &object_id, own_properties);
                let _ = reply_tx.send(result);
            }
            WorkerMessage::AwaitPromise {
                object_id,
                return_by_value,
                reply_tx,
            } => {
                let result = await_promise(scope, &mut slot.registry, &object_id, return_by_value);
                let _ = reply_tx.send(result);
            }
            WorkerMessage::ReleaseObject {
                object_id,
                reply_tx,
            } => {
                slot.registry.release(&object_id);
                let _ = reply_tx.send(Ok(()));
            }
            WorkerMessage::ReleaseObjectGroup {
                object_group,
                reply_tx,
            } => {
                slot.registry.release_group(&object_group);
                let _ = reply_tx.send(Ok(()));
            }
            WorkerMessage::ResolveNode { source, reply_tx } => {
                let result = eval_in_scope(scope, &source, false, &mut slot.registry);
                let _ = reply_tx.send(result);
            }
            WorkerMessage::Shutdown => {}
        }
    }
    if let Some(state) = take_op_state() {
        slot.op_rx = Some(state.op_rx);
    }
}

fn reply_missing(msg: &WorkerMessage) {
    match msg {
        WorkerMessage::Eval { reply_tx, .. }
        | WorkerMessage::CallFunctionOn { reply_tx, .. }
        | WorkerMessage::AwaitPromise { reply_tx, .. }
        | WorkerMessage::ResolveNode { reply_tx, .. } => {
            let _ = reply_tx.send(Err("isolate not found".into()));
        }
        WorkerMessage::GetProperties { reply_tx, .. } => {
            let _ = reply_tx.send(Err("isolate not found".into()));
        }
        WorkerMessage::ReleaseObject { reply_tx, .. }
        | WorkerMessage::ReleaseObjectGroup { reply_tx, .. } => {
            let _ = reply_tx.send(Err("isolate not found".into()));
        }
        WorkerMessage::Shutdown => {}
    }
}
