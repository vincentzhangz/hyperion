//! Shared configuration for Hyperion servers.
//!
//! Values come from environment variables (`HYPERION_*`) and can be overridden
//! by CLI flags. Secrets are never printed via [`Debug`].

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use subtle::ConstantTimeEq;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
}

/// Top-level process configuration.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub node_id: String,
    pub auth_token: Option<String>,
    pub allow_private_network: bool,
    pub max_header_bytes: usize,
    pub max_ws_message_bytes: usize,
    pub max_body_bytes: usize,
    pub fetch_timeout: Duration,
    pub fetch_pause_timeout: Duration,
    pub shutdown_grace: Duration,
    pub max_targets: usize,
    pub max_sessions: usize,
    pub max_previews: usize,
    pub max_remote_objects: usize,
    pub max_response_cache_bytes: usize,
    pub max_active_navigations: usize,
    pub max_active_evals: usize,
    pub max_queued_commands: usize,
    pub per_origin_connections: usize,
    pub idle_target_ttl: Duration,
    pub workers: usize,
    pub v8_pool_size: usize,
    pub v8_heap_mb: usize,
    pub eval_deadline: Duration,
    pub log_level: String,
    pub log_format: String,
    pub profile_root: PathBuf,
    pub profile_backend: ProfileBackend,
    pub s3: S3Config,
    pub postgres_url: Option<String>,
    pub encryption_key: Option<Vec<u8>>,
    pub dashboard_bind: Option<SocketAddr>,
    pub preview_origin: Option<String>,
    pub prometheus_url: Option<String>,
    pub session_ttl: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileBackend {
    Local,
    S3,
}

#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("node_id", &self.node_id)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[redacted]"),
            )
            .field("allow_private_network", &self.allow_private_network)
            .field("max_header_bytes", &self.max_header_bytes)
            .field("max_ws_message_bytes", &self.max_ws_message_bytes)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_targets", &self.max_targets)
            .field("workers", &self.workers)
            .field("profile_backend", &self.profile_backend)
            .field(
                "postgres_url",
                &self.postgres_url.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "encryption_key",
                &self.encryption_key.as_ref().map(|_| "[redacted]"),
            )
            .finish_non_exhaustive()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9222".parse().unwrap(),
            node_id: hostname_or_random(),
            auth_token: None,
            allow_private_network: false,
            max_header_bytes: 64 * 1024,
            max_ws_message_bytes: 8 * 1024 * 1024,
            max_body_bytes: 16 * 1024 * 1024,
            fetch_timeout: Duration::from_secs(30),
            fetch_pause_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(15),
            max_targets: 2048,
            max_sessions: 4096,
            max_previews: 256,
            max_remote_objects: 10_000,
            max_response_cache_bytes: 64 * 1024 * 1024,
            max_active_navigations: 128,
            max_active_evals: 64,
            max_queued_commands: 1024,
            per_origin_connections: 8,
            idle_target_ttl: Duration::from_secs(30 * 60),
            workers: 1,
            v8_pool_size: 4,
            v8_heap_mb: 128,
            eval_deadline: Duration::from_secs(10),
            log_level: "info".into(),
            log_format: "json".into(),
            profile_root: PathBuf::from("/var/lib/hyperion/profiles"),
            profile_backend: ProfileBackend::Local,
            s3: S3Config {
                region: "us-east-1".into(),
                ..Default::default()
            },
            postgres_url: None,
            encryption_key: None,
            dashboard_bind: None,
            preview_origin: None,
            prometheus_url: None,
            session_ttl: Duration::from_secs(8 * 3600),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut c = Self::default();
        if let Some(v) = env("HYPERION_BIND") {
            c.bind = v
                .parse()
                .map_err(|e| ConfigError::Invalid(format!("HYPERION_BIND: {e}")))?;
        }
        if let Some(v) = env("HYPERION_NODE_ID") {
            c.node_id = v;
        }
        if let Some(v) = env("HYPERION_AUTH_TOKEN")
            && !v.is_empty()
        {
            c.auth_token = Some(v);
        }
        if let Some(v) = env("HYPERION_ALLOW_PRIVATE_NETWORK") {
            c.allow_private_network = parse_bool(&v)?;
        }
        if let Some(v) = env("HYPERION_MAX_HEADER_BYTES") {
            c.max_header_bytes = parse_usize(&v, "HYPERION_MAX_HEADER_BYTES")?;
        }
        if let Some(v) = env("HYPERION_MAX_WS_MESSAGE_BYTES") {
            c.max_ws_message_bytes = parse_usize(&v, "HYPERION_MAX_WS_MESSAGE_BYTES")?;
        }
        if let Some(v) = env("HYPERION_MAX_BODY_BYTES") {
            c.max_body_bytes = parse_usize(&v, "HYPERION_MAX_BODY_BYTES")?;
        }
        if let Some(v) = env("HYPERION_FETCH_TIMEOUT_SECS") {
            c.fetch_timeout = Duration::from_secs(parse_u64(&v, "HYPERION_FETCH_TIMEOUT_SECS")?);
        }
        if let Some(v) = env("HYPERION_FETCH_PAUSE_TIMEOUT_SECS") {
            c.fetch_pause_timeout =
                Duration::from_secs(parse_u64(&v, "HYPERION_FETCH_PAUSE_TIMEOUT_SECS")?);
        }
        if let Some(v) = env("HYPERION_SHUTDOWN_GRACE_SECS") {
            c.shutdown_grace = Duration::from_secs(parse_u64(&v, "HYPERION_SHUTDOWN_GRACE_SECS")?);
        }
        if let Some(v) = env("HYPERION_MAX_TARGETS") {
            c.max_targets = parse_usize(&v, "HYPERION_MAX_TARGETS")?;
        }
        if let Some(v) = env("HYPERION_MAX_SESSIONS") {
            c.max_sessions = parse_usize(&v, "HYPERION_MAX_SESSIONS")?;
        }
        if let Some(v) = env("HYPERION_MAX_PREVIEWS") {
            c.max_previews = parse_usize(&v, "HYPERION_MAX_PREVIEWS")?;
        }
        if let Some(v) = env("HYPERION_MAX_REMOTE_OBJECTS") {
            c.max_remote_objects = parse_usize(&v, "HYPERION_MAX_REMOTE_OBJECTS")?;
        }
        if let Some(v) = env("HYPERION_MAX_RESPONSE_CACHE_BYTES") {
            c.max_response_cache_bytes = parse_usize(&v, "HYPERION_MAX_RESPONSE_CACHE_BYTES")?;
        }
        if let Some(v) = env("HYPERION_MAX_ACTIVE_NAVIGATIONS") {
            c.max_active_navigations = parse_usize(&v, "HYPERION_MAX_ACTIVE_NAVIGATIONS")?;
        }
        if let Some(v) = env("HYPERION_MAX_ACTIVE_EVALS") {
            c.max_active_evals = parse_usize(&v, "HYPERION_MAX_ACTIVE_EVALS")?;
        }
        if let Some(v) = env("HYPERION_MAX_QUEUED_COMMANDS") {
            c.max_queued_commands = parse_usize(&v, "HYPERION_MAX_QUEUED_COMMANDS")?;
        }
        if let Some(v) = env("HYPERION_PER_ORIGIN_CONNECTIONS") {
            c.per_origin_connections = parse_usize(&v, "HYPERION_PER_ORIGIN_CONNECTIONS")?;
        }
        if let Some(v) = env("HYPERION_IDLE_TARGET_TTL_SECS") {
            c.idle_target_ttl =
                Duration::from_secs(parse_u64(&v, "HYPERION_IDLE_TARGET_TTL_SECS")?);
        }
        if let Some(v) = env("HYPERION_WORKERS") {
            c.workers = parse_usize(&v, "HYPERION_WORKERS")?.max(1);
        }
        if let Some(v) = env("HYPERION_V8_POOL_SIZE") {
            c.v8_pool_size = parse_usize(&v, "HYPERION_V8_POOL_SIZE")?.max(1);
        }
        if let Some(v) = env("HYPERION_V8_HEAP_MB") {
            c.v8_heap_mb = parse_usize(&v, "HYPERION_V8_HEAP_MB")?;
        }
        if let Some(v) = env("HYPERION_EVAL_DEADLINE_SECS") {
            c.eval_deadline = Duration::from_secs(parse_u64(&v, "HYPERION_EVAL_DEADLINE_SECS")?);
        }
        if let Some(v) = env("HYPERION_LOG_LEVEL") {
            c.log_level = v;
        }
        if let Some(v) = env("HYPERION_LOG_FORMAT") {
            c.log_format = v;
        }
        if let Some(v) = env("HYPERION_PROFILE_ROOT") {
            c.profile_root = PathBuf::from(v);
        }
        if let Some(v) = env("HYPERION_PROFILE_BACKEND") {
            c.profile_backend = match v.to_ascii_lowercase().as_str() {
                "local" => ProfileBackend::Local,
                "s3" => ProfileBackend::S3,
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "HYPERION_PROFILE_BACKEND: expected local|s3, got {other}"
                    )));
                }
            };
        }
        c.s3.endpoint = env("HYPERION_S3_ENDPOINT");
        c.s3.bucket = env("HYPERION_S3_BUCKET");
        if let Some(v) = env("HYPERION_S3_REGION") {
            c.s3.region = v;
        }
        c.s3.access_key = env("HYPERION_S3_ACCESS_KEY");
        c.s3.secret_key = env("HYPERION_S3_SECRET_KEY");
        c.postgres_url = env("HYPERION_POSTGRES_URL");
        if let Some(v) = env("HYPERION_ENCRYPTION_KEY") {
            c.encryption_key = Some(parse_key(&v)?);
        } else if let Some(path) = env("HYPERION_ENCRYPTION_KEY_FILE") {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::Invalid(format!("read {path}: {e}")))?;
            c.encryption_key = Some(parse_key(raw.trim())?);
        }
        if let Some(v) = env("HYPERION_DASHBOARD_BIND") {
            c.dashboard_bind = Some(
                v.parse()
                    .map_err(|e| ConfigError::Invalid(format!("HYPERION_DASHBOARD_BIND: {e}")))?,
            );
        }
        c.preview_origin = env("HYPERION_PREVIEW_ORIGIN");
        c.prometheus_url = env("HYPERION_PROMETHEUS_URL");
        if let Some(v) = env("HYPERION_SESSION_TTL_SECS") {
            c.session_ttl = Duration::from_secs(parse_u64(&v, "HYPERION_SESSION_TTL_SECS")?);
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !is_loopback(self.bind.ip()) && self.auth_token.is_none() {
            return Err(ConfigError::Invalid(
                "non-loopback bind requires HYPERION_AUTH_TOKEN".into(),
            ));
        }
        if self.profile_backend == ProfileBackend::S3
            && (self.s3.bucket.is_none() || self.s3.access_key.is_none())
        {
            return Err(ConfigError::Invalid(
                "S3 backend requires HYPERION_S3_BUCKET and HYPERION_S3_ACCESS_KEY".into(),
            ));
        }
        Ok(())
    }

    /// Constant-time comparison against the configured bearer token.
    pub fn token_matches(&self, presented: &str) -> bool {
        let Some(expected) = self.auth_token.as_deref() else {
            return is_loopback(self.bind.ip());
        };
        if presented.len() != expected.len() {
            // Still compare to keep timing flatter.
            let _ = expected.as_bytes().ct_eq(expected.as_bytes());
            return false;
        }
        bool::from(expected.as_bytes().ct_eq(presented.as_bytes()))
    }

    pub fn auth_required(&self) -> bool {
        self.auth_token.is_some() || !is_loopback(self.bind.ip())
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback(),
        IpAddr::V6(v) => v.is_loopback(),
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn parse_bool(v: &str) -> Result<bool, ConfigError> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Invalid(format!("invalid bool: {v}"))),
    }
}

fn parse_usize(v: &str, name: &str) -> Result<usize, ConfigError> {
    v.parse()
        .map_err(|_| ConfigError::Invalid(format!("{name}: not an integer")))
}

fn parse_u64(v: &str, name: &str) -> Result<u64, ConfigError> {
    v.parse()
        .map_err(|_| ConfigError::Invalid(format!("{name}: not an integer")))
}

fn parse_key(v: &str) -> Result<Vec<u8>, ConfigError> {
    if let Ok(bytes) = hex::decode_loose(v)
        && bytes.len() == 32
    {
        return Ok(bytes);
    }
    let bytes = v.as_bytes();
    if bytes.len() >= 32 {
        return Ok(bytes[..32].to_vec());
    }
    Err(ConfigError::Invalid(
        "encryption key must be 32 bytes or 64 hex chars".into(),
    ))
}

fn hostname_or_random() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("node-{}", std::process::id()))
}

mod hex {
    pub fn decode_loose(s: &str) -> Result<Vec<u8>, ()> {
        let s = s.trim();
        if !s.len().is_multiple_of(2) {
            return Err(());
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        for i in (0..bytes.len()).step_by(2) {
            let hi = from_hex(bytes[i])?;
            let lo = from_hex(bytes[i + 1])?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn from_hex(b: u8) -> Result<u8, ()> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_without_token_is_ok() {
        let c = Config::default();
        assert!(c.validate().is_ok());
        assert!(c.token_matches("anything") || !c.auth_required());
    }

    #[test]
    fn non_loopback_requires_token() {
        let mut c = Config::default();
        c.bind = "0.0.0.0:9222".parse().unwrap();
        assert!(c.validate().is_err());
        c.auth_token = Some("secret-token-value-32chars!!!!".into());
        assert!(c.validate().is_ok());
        assert!(c.token_matches("secret-token-value-32chars!!!!"));
        assert!(!c.token_matches("wrong"));
    }
}
