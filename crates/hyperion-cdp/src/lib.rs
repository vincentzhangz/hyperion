//! hyperion-cdp: a Chrome DevTools Protocol server.
//!
//! Everything runs on a single Glommio executor thread (`Page` is `!Send`
//! because it wraps the V8 isolate). A single "browser" task owns the target
//! and session registry and executes every command; per-connection tasks only
//! do socket I/O. Messages travel over glommio `local_channel`s, so command
//! execution is fully serialized and can `await` a navigation without holding
//! a `RefCell` borrow.
//!
//! Layout:
//! - [`protocol`]   – CDP message types and error codes
//! - [`ws`]         – hand-rolled RFC6455 frame codec over a shared TCP stream
//! - [`connection`] – HTTP head parsing, `/json/*` + `/devtools/*` + `/preview/*`
//! - [`preview`]    – live DOM/HTML content mirror viewer
//! - [`server`]     – the `CdpServer` browser task: registry, dispatch, events

pub mod connection;
pub mod preview;
pub mod protocol;
pub mod server;
pub mod ws;

pub use connection::{CdpRequest, OutMsg};
pub use protocol::{ErrorObj, Event, Request, Response};
pub use server::{CdpServer, CdpServerOptions};
