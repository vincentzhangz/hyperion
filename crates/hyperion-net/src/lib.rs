//! hyperion-net: networking layer on Glommio (io_uring).
//!
//! HTTP client built on `hyper` over `glommio::net::TcpStream` (no tokio).

pub mod client;
pub mod connector;
pub mod cookie;
pub mod error;
pub mod ssrf;

pub use client::{
    FetchAction, FetchIntercept, FetchPausedSink, HttpClient, HttpClientOptions, HttpResponse,
    NetEvent, NetEventSink,
};
pub use cookie::{CookieEntry, CookieJar};
pub use error::Error;
