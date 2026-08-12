//! Lock-free-ish process metrics rendered as Prometheus text.
//!
//! Counters and gauges are atomics so Glommio workers and the V8 pool can
//! record without taking locks on the hot path.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn global() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

pub struct Metrics {
    pub targets_open: AtomicU64,
    pub targets_active: AtomicU64,
    pub targets_idle: AtomicU64,
    pub sessions: AtomicU64,
    pub previews: AtomicU64,
    pub profiles_loaded: AtomicU64,
    pub profiles_dirty: AtomicU64,
    pub navigations_ok: AtomicU64,
    pub navigations_err: AtomicU64,
    pub commands: AtomicU64,
    pub evals: AtomicU64,
    pub admission_rejected: AtomicU64,
    pub queue_depth: AtomicU64,
    pub queue_drops: AtomicU64,
    pub fetch_pauses: AtomicU64,
    pub fetch_timeouts: AtomicU64,
    pub cache_bytes: AtomicU64,
    pub object_registry: AtomicU64,
    pub v8_heap_bytes: AtomicU64,
    pub rss_bytes: AtomicU64,
    pub lease_contention: AtomicU64,
    pub profile_flush_ok: AtomicU64,
    pub profile_flush_err: AtomicU64,
    pub shutdown: AtomicU64,
    pub nav_latency_ms_sum: AtomicU64,
    pub nav_latency_ms_count: AtomicU64,
    pub cmd_latency_ms_sum: AtomicU64,
    pub cmd_latency_ms_count: AtomicU64,
    pub http_pool_reuse: AtomicU64,
    pub origin_inflight: AtomicI64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            targets_open: AtomicU64::new(0),
            targets_active: AtomicU64::new(0),
            targets_idle: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            previews: AtomicU64::new(0),
            profiles_loaded: AtomicU64::new(0),
            profiles_dirty: AtomicU64::new(0),
            navigations_ok: AtomicU64::new(0),
            navigations_err: AtomicU64::new(0),
            commands: AtomicU64::new(0),
            evals: AtomicU64::new(0),
            admission_rejected: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            queue_drops: AtomicU64::new(0),
            fetch_pauses: AtomicU64::new(0),
            fetch_timeouts: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            object_registry: AtomicU64::new(0),
            v8_heap_bytes: AtomicU64::new(0),
            rss_bytes: AtomicU64::new(0),
            lease_contention: AtomicU64::new(0),
            profile_flush_ok: AtomicU64::new(0),
            profile_flush_err: AtomicU64::new(0),
            shutdown: AtomicU64::new(0),
            nav_latency_ms_sum: AtomicU64::new(0),
            nav_latency_ms_count: AtomicU64::new(0),
            cmd_latency_ms_sum: AtomicU64::new(0),
            cmd_latency_ms_count: AtomicU64::new(0),
            http_pool_reuse: AtomicU64::new(0),
            origin_inflight: AtomicI64::new(0),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            targets_open: self.targets_open.load(Ordering::Relaxed),
            targets_active: self.targets_active.load(Ordering::Relaxed),
            targets_idle: self.targets_idle.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            previews: self.previews.load(Ordering::Relaxed),
            profiles_loaded: self.profiles_loaded.load(Ordering::Relaxed),
            profiles_dirty: self.profiles_dirty.load(Ordering::Relaxed),
            navigations_ok: self.navigations_ok.load(Ordering::Relaxed),
            navigations_err: self.navigations_err.load(Ordering::Relaxed),
            commands: self.commands.load(Ordering::Relaxed),
            evals: self.evals.load(Ordering::Relaxed),
            admission_rejected: self.admission_rejected.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            queue_drops: self.queue_drops.load(Ordering::Relaxed),
            fetch_pauses: self.fetch_pauses.load(Ordering::Relaxed),
            fetch_timeouts: self.fetch_timeouts.load(Ordering::Relaxed),
            cache_bytes: self.cache_bytes.load(Ordering::Relaxed),
            object_registry: self.object_registry.load(Ordering::Relaxed),
            v8_heap_bytes: self.v8_heap_bytes.load(Ordering::Relaxed),
            rss_bytes: self.rss_bytes.load(Ordering::Relaxed),
            lease_contention: self.lease_contention.load(Ordering::Relaxed),
            profile_flush_ok: self.profile_flush_ok.load(Ordering::Relaxed),
            profile_flush_err: self.profile_flush_err.load(Ordering::Relaxed),
            shutdown: self.shutdown.load(Ordering::Relaxed),
            nav_latency_ms_avg: avg(
                self.nav_latency_ms_sum.load(Ordering::Relaxed),
                self.nav_latency_ms_count.load(Ordering::Relaxed),
            ),
            cmd_latency_ms_avg: avg(
                self.cmd_latency_ms_sum.load(Ordering::Relaxed),
                self.cmd_latency_ms_count.load(Ordering::Relaxed),
            ),
            http_pool_reuse: self.http_pool_reuse.load(Ordering::Relaxed),
            origin_inflight: self.origin_inflight.load(Ordering::Relaxed).max(0) as u64,
            ts_ms: now_ms(),
        }
    }

    pub fn render_prometheus(&self) -> String {
        let s = self.snapshot();
        let mut out = String::with_capacity(2048);
        line(&mut out, "hyperion_targets_open", s.targets_open);
        line(&mut out, "hyperion_targets_active", s.targets_active);
        line(&mut out, "hyperion_targets_idle", s.targets_idle);
        line(&mut out, "hyperion_sessions", s.sessions);
        line(&mut out, "hyperion_previews", s.previews);
        line(&mut out, "hyperion_profiles_loaded", s.profiles_loaded);
        line(&mut out, "hyperion_profiles_dirty", s.profiles_dirty);
        line(&mut out, "hyperion_navigations_ok_total", s.navigations_ok);
        line(
            &mut out,
            "hyperion_navigations_err_total",
            s.navigations_err,
        );
        line(&mut out, "hyperion_commands_total", s.commands);
        line(&mut out, "hyperion_evals_total", s.evals);
        line(
            &mut out,
            "hyperion_admission_rejected_total",
            s.admission_rejected,
        );
        line(&mut out, "hyperion_queue_depth", s.queue_depth);
        line(&mut out, "hyperion_queue_drops_total", s.queue_drops);
        line(&mut out, "hyperion_fetch_pauses_total", s.fetch_pauses);
        line(&mut out, "hyperion_fetch_timeouts_total", s.fetch_timeouts);
        line(&mut out, "hyperion_cache_bytes", s.cache_bytes);
        line(&mut out, "hyperion_object_registry", s.object_registry);
        line(&mut out, "hyperion_v8_heap_bytes", s.v8_heap_bytes);
        line(&mut out, "hyperion_rss_bytes", s.rss_bytes);
        line(
            &mut out,
            "hyperion_lease_contention_total",
            s.lease_contention,
        );
        line(
            &mut out,
            "hyperion_profile_flush_ok_total",
            s.profile_flush_ok,
        );
        line(
            &mut out,
            "hyperion_profile_flush_err_total",
            s.profile_flush_err,
        );
        line(&mut out, "hyperion_shutdown", s.shutdown);
        line(
            &mut out,
            "hyperion_http_pool_reuse_total",
            s.http_pool_reuse,
        );
        line(&mut out, "hyperion_origin_inflight", s.origin_inflight);
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Snapshot {
    pub targets_open: u64,
    pub targets_active: u64,
    pub targets_idle: u64,
    pub sessions: u64,
    pub previews: u64,
    pub profiles_loaded: u64,
    pub profiles_dirty: u64,
    pub navigations_ok: u64,
    pub navigations_err: u64,
    pub commands: u64,
    pub evals: u64,
    pub admission_rejected: u64,
    pub queue_depth: u64,
    pub queue_drops: u64,
    pub fetch_pauses: u64,
    pub fetch_timeouts: u64,
    pub cache_bytes: u64,
    pub object_registry: u64,
    pub v8_heap_bytes: u64,
    pub rss_bytes: u64,
    pub lease_contention: u64,
    pub profile_flush_ok: u64,
    pub profile_flush_err: u64,
    pub shutdown: u64,
    pub nav_latency_ms_avg: f64,
    pub cmd_latency_ms_avg: f64,
    pub http_pool_reuse: u64,
    pub origin_inflight: u64,
    pub ts_ms: u64,
}

fn line(out: &mut String, name: &str, v: u64) {
    out.push_str(name);
    out.push(' ');
    out.push_str(&v.to_string());
    out.push('\n');
}

fn avg(sum: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Refresh RSS from `/proc/self/statm` and record a configured V8 heap budget.
pub fn refresh_process_gauges(v8_heap_budget_bytes: u64) {
    let m = global();
    m.v8_heap_bytes
        .store(v8_heap_budget_bytes, Ordering::Relaxed);
    m.rss_bytes.store(read_rss_bytes(), Ordering::Relaxed);
}

fn read_rss_bytes() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok());
    pages.unwrap_or(0).saturating_mul(4096)
}
