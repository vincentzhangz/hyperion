//! hyperion-js: V8 JavaScript runtime.
//!
//! Isolates live on dedicated worker threads and talk to the Glommio
//! executor over channels. Uses the `v8` crate directly (no deno_core, no tokio).

pub mod pool;
pub mod runtime;

pub use pool::{configure as configure_v8_pool, live_isolates};
pub use runtime::{EvalOutcome, JsRuntime, OpRequest, OpResponse, PropertyDescriptor};

/// DOM/window prelude installed after the host registers the DOM op (id 1).
pub const DOM_PRELUDE: &str = include_str!("prelude.js");

/// Run a trivial script and report the V8 version. Used as a build/runtime
/// smoke test to validate that the prebuilt V8 archive links correctly.
pub fn v8_smoke() {
    let _platform = runtime::platform();
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());

    v8::scope!(let handle_scope, isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &v8::ContextScope::new(handle_scope, context);

    let code = v8::String::new(scope, "'hello from V8 ' + (6 * 7)").unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    let result = script.run(scope).unwrap();
    let s = result.to_string(scope).unwrap();
    println!("v8: {}", s.to_rust_string_lossy(scope));
    println!("v8 version: {}", v8::V8::get_version());
}

/// The linked V8 version string (no isolate required).
pub fn v8_version() -> String {
    v8::V8::get_version().to_owned()
}
