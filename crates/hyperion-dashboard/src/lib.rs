//! Cluster operations dashboard: session auth, JSON APIs, embedded SPA.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperion_config::Config;
use hyperion_metrics::Snapshot;
use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

const INDEX_HTML: &str = include_str!("../frontend/dist/index.html");
const APP_JS: &str = include_str!("../frontend/dist/app.js");
const APP_CSS: &str = include_str!("../frontend/dist/app.css");

#[derive(Clone)]
pub struct DashboardState {
    inner: Arc<Mutex<Inner>>,
    config: Config,
}

struct Inner {
    sessions: HashMap<String, DashSession>,
    events: VecDeque<AuditEvent>,
}

struct DashSession {
    csrf: String,
    expires: Instant,
}

#[derive(Clone, Serialize)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub level: String,
    pub message: String,
    pub correlation_id: String,
}

impl DashboardState {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sessions: HashMap::new(),
                events: VecDeque::with_capacity(512),
            })),
            config,
        }
    }

    pub fn push_event(&self, level: &str, message: &str, correlation_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.events.len() >= 500 {
            inner.events.pop_front();
        }
        inner.events.push_back(AuditEvent {
            ts_ms: now_ms(),
            level: level.to_owned(),
            message: message.to_owned(),
            correlation_id: correlation_id.to_owned(),
        });
    }

    pub fn login(&self, bearer: &str) -> Option<(String, String)> {
        if self.config.auth_required() && !self.config.token_matches(bearer) {
            return None;
        }
        let sid = random_hex(16);
        let csrf = random_hex(16);
        self.inner.lock().unwrap().sessions.insert(
            sid.clone(),
            DashSession {
                csrf: csrf.clone(),
                expires: Instant::now() + self.config.session_ttl,
            },
        );
        Some((sid, csrf))
    }

    pub fn session_ok(&self, cookie: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        inner.sessions.retain(|_, s| s.expires > Instant::now());
        inner.sessions.get(cookie).map(|s| s.csrf.clone())
    }

    pub fn csrf_ok(&self, cookie: &str, csrf: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.sessions.get(cookie) {
            Some(s) if s.expires > Instant::now() => {
                s.csrf.as_bytes().ct_eq(csrf.as_bytes()).into()
            }
            _ => false,
        }
    }

    pub fn events(&self) -> Vec<AuditEvent> {
        self.inner.lock().unwrap().events.iter().cloned().collect()
    }

    pub fn index_html(&self) -> &'static str {
        INDEX_HTML
    }

    pub fn app_js(&self) -> &'static str {
        APP_JS
    }

    pub fn app_css(&self) -> &'static str {
        APP_CSS
    }

    pub fn overview_json(&self, extra: Value) -> Value {
        let snap: Snapshot = hyperion_metrics::global().snapshot();
        json!({
            "nodeId": self.config.node_id,
            "metrics": snap,
            "stale": extra.get("stale").cloned().unwrap_or(json!(false)),
            "workers": extra.get("workers").cloned().unwrap_or(json!([])),
            "profiles": extra.get("profiles").cloned().unwrap_or(json!([])),
            "pages": extra.get("pages").cloned().unwrap_or(json!([])),
        })
    }
}

fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse `hyperion_session` from a Cookie header.
pub fn session_from_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("hyperion_session=")
            .map(|s| s.trim().to_owned())
    })
}

pub fn login_set_cookie(sid: &str, ttl: Duration, secure: bool) -> String {
    let secure_flag = if secure { "; Secure" } else { "" };
    format!(
        "hyperion_session={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure_flag}",
        ttl.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_issues_csrf_and_rejects_bad_token() {
        let mut config = Config::default();
        config.auth_token = Some("dashboard-secret-token-value!!".into());
        let dash = DashboardState::new(config);
        assert!(dash.login("wrong").is_none());
        let (sid, csrf) = dash.login("dashboard-secret-token-value!!").unwrap();
        assert_eq!(dash.session_ok(&sid).as_deref(), Some(csrf.as_str()));
        assert!(dash.csrf_ok(&sid, &csrf));
        assert!(!dash.csrf_ok(&sid, "nope"));
        assert_eq!(
            session_from_cookie("hyperion_session=abc; other=1"),
            Some("abc".into())
        );
        let cookie = login_set_cookie("abc", Duration::from_secs(60), false);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("Secure"));
    }
}
