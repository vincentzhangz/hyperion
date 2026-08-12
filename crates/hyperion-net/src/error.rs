use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
    #[error("url has no host")]
    NoHost,
    #[error("unsupported url scheme: {0}")]
    Scheme(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] http::Error),
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("tls error: {0}")]
    Tls(String),
    #[error("request timed out")]
    Timeout,
    #[error("blocked destination: {0}")]
    Blocked(String),
    #[error("response body exceeds limit")]
    BodyTooLarge,
    #[error("fetch interception timed out")]
    FetchPausedTimeout,
    #[error("overloaded: {0}")]
    Overload(String),
}

pub type Result<T> = std::result::Result<T, Error>;
