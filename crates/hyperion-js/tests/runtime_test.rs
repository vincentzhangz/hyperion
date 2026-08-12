//! Integration tests for the V8 worker runtime and its synchronous op bridge.

use hyperion_js::JsRuntime;

#[test]
fn evaluate_basic_expressions() {
    let mut rt = JsRuntime::new();
    assert_eq!(rt.evaluate("1 + 2").unwrap(), "3");
    assert_eq!(rt.evaluate("'hello ' + 'world'").unwrap(), "hello world");
    assert_eq!(rt.evaluate("(6 * 7)").unwrap(), "42");
    assert_eq!(rt.evaluate("[1,2,3].length").unwrap(), "3");
    assert_eq!(rt.evaluate("Math.max(1, 5, 3)").unwrap(), "5");
}

#[test]
fn evaluate_persists_state_across_calls() {
    let mut rt = JsRuntime::new();
    rt.evaluate("globalThis.counter = 0").unwrap();
    rt.evaluate("globalThis.counter += 2").unwrap();
    rt.evaluate("globalThis.counter += 3").unwrap();
    assert_eq!(rt.evaluate("globalThis.counter").unwrap(), "5");
}

#[test]
fn evaluate_compile_error() {
    let mut rt = JsRuntime::new();
    assert!(rt.evaluate("function (").is_err());
}

#[test]
fn op_bridge_round_trip() {
    let mut rt = JsRuntime::new();
    rt.register_op(1, |arg| {
        let n: i64 = arg.trim().parse().expect("numeric arg");
        format!(r#"{{"doubled":{}}}"#, n * 2)
    });

    let out = rt
        .evaluate(r#"JSON.parse(__hyperion_op(1, "21")).doubled"#)
        .unwrap();
    assert_eq!(out, "42");
}

#[test]
fn op_bridge_multiple_ops_and_state() {
    let mut rt = JsRuntime::new();
    rt.register_op(2, |arg| {
        format!(r#"{{"uppercase":"{}"}}"#, arg.to_uppercase())
    });

    let out = rt
        .evaluate(r#"JSON.parse(__hyperion_op(2, "hyperion")).uppercase"#)
        .unwrap();
    assert_eq!(out, "HYPERION");
}

#[test]
fn op_bridge_unknown_op_returns_error() {
    let mut rt = JsRuntime::new();
    let out = rt
        .evaluate(r#"JSON.parse(__hyperion_op(999, "{}")).error"#)
        .unwrap();
    assert!(out.contains("unknown op id 999"), "got: {out}");
}

#[test]
fn op_can_chain_with_js_logic() {
    let mut rt = JsRuntime::new();
    rt.register_op(3, |arg| {
        let vals: Vec<i64> = serde_json::from_str(arg).unwrap_or_default();
        let sum: i64 = vals.iter().sum();
        format!(r#"{{"sum":{sum}}}"#)
    });

    let script = r#"
        const args = JSON.stringify([1, 2, 3, 4]);
        const result = JSON.parse(__hyperion_op(3, args));
        result.sum * 2
    "#;
    assert_eq!(rt.evaluate(script).unwrap(), "20");
}

#[test]
fn evaluate_outcome_reports_exception() {
    let mut rt = JsRuntime::new();
    let ok = rt.evaluate_outcome("40 + 2").unwrap();
    assert!(!ok.is_exception);
    assert_eq!(ok.value, "42");

    let bad = rt.evaluate_outcome("throw new Error('boom')").unwrap();
    assert!(bad.is_exception);
    assert!(bad.value.is_empty());
    let exc = bad.exception.unwrap();
    assert!(exc.contains("boom"), "exception text: {exc}");
}

#[test]
fn console_log_reaches_op_zero() {
    let mut rt = JsRuntime::new();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls_clone = calls.clone();
    rt.register_op(0, move |arg| {
        calls_clone.lock().unwrap().push(arg.to_owned());
        "{}".to_owned()
    });

    let _ = rt.evaluate("console.log('hello', 42); 1 + 1");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "console.log should emit one op-0 call");
    let payload: serde_json::Value = serde_json::from_str(&calls[0]).unwrap();
    assert_eq!(payload["type"], "log");
    assert_eq!(payload["args"][0], "hello");
    assert_eq!(payload["args"][1], "42");
}

#[test]
fn two_runtimes_on_shared_pool() {
    let mut a = JsRuntime::new();
    let mut b = JsRuntime::new();
    a.evaluate("globalThis.mark = 'a'").unwrap();
    b.evaluate("globalThis.mark = 'b'").unwrap();
    assert_eq!(a.evaluate("globalThis.mark").unwrap(), "a");
    assert_eq!(b.evaluate("globalThis.mark").unwrap(), "b");
    assert_eq!(a.evaluate("1 + 1").unwrap(), "2");
    assert_eq!(b.evaluate("2 + 2").unwrap(), "4");
}

#[test]
fn console_error_does_not_break_scripts() {
    let mut rt = JsRuntime::new();
    // No op-0 handler registered: console calls must be swallowed silently.
    let out = rt.evaluate("console.warn('x'); 7 * 6").unwrap();
    assert_eq!(out, "42");
}
