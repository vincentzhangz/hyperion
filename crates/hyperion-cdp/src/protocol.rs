//! Chrome DevTools Protocol message types and JSON-RPC error codes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Parse error (JSON-RPC 2.0).
pub const PARSE_ERROR: i64 = -32700;
/// Invalid Request (JSON-RPC 2.0).
pub const INVALID_REQUEST: i64 = -32600;
/// Method not found (JSON-RPC 2.0).
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid method parameter(s).
pub const INVALID_PARAMS: i64 = -32602;
/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;
/// Server error (CDP `-32000`, e.g. "not attached to a page").
pub const SERVER_ERROR: i64 = -32000;

/// An incoming command from a client.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub id: i64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Present for flattened (attached) sessions.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// A response to a [`Request`]. Exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
}

/// A protocol error object.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorObj {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A server-initiated event.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// Set for events belonging to a flattened (attached) session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ErrorObj {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// CDP `-32601 Method not found`.
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("Method '{method}' not implemented"),
        )
    }

    /// CDP `-32602 Invalid method parameter(s)`.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    /// CDP `-32000` server error.
    pub fn server(message: impl Into<String>) -> Self {
        Self::new(SERVER_ERROR, message)
    }

    /// Admission-control rejection with retry guidance.
    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(SERVER_ERROR, format!("Overloaded: {}", message.into()))
    }
}
