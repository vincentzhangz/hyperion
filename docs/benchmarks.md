# Benchmarks and stress testing

There are **no published 2,000-target numbers** in this repository. Do not copy latency or RSS figures from another host. Soak results are hardware-specific (CPU, memlock, disk, network).

Reference *shape* (not a measured result): one Linux node, `HYPERION_WORKERS` sized to cores you actually run, `HYPERION_V8_POOL_SIZE` = 4–8, `HYPERION_MAX_TARGETS=2048`.

## Harness

`scripts/stress/run.sh` + `scripts/stress/driver.mjs` (Node 20+). The old `scripts/bench-open-targets.sh` wrapper calls the same smoke entrypoint.

Modes:

| Mode | Default targets | Extra opens | Hold | Notes |
|------|-----------------|-------------|------|-------|
| `smoke` | 8 | 8 | 2s | Idle opens, nav/eval sample, WS churn, cleanup |
| `selftest` | 2 | 4 | 1s | **Fails** unless the server returns `Overloaded:` |
| `soak` | 2000 | 16 | 300s in CI | Raise `STRESS_HOLD_SECS` for multi-hour local soaks |

The driver also checks `/health/live`, scrapes `/metrics` (including `hyperion_rss_bytes`), records client-side p50/p95/p99, and writes `artifacts/stress-*/summary.json` + `summary.txt` (gitignored).

Environment: `HOST`, `HYPERION_AUTH_TOKEN`, `STRESS_TARGETS`, `STRESS_EXTRA`, `STRESS_HOLD_SECS`, `STRESS_NAV_EVAL`, `STRESS_CHURN`, `STRESS_ORIGIN_HOST` (use `host.docker.internal` when Hyperion is in Docker and the driver serves HTML on the host), `STRESS_FAIL_P99_MS`, `STRESS_FAIL_CLEANUP_REMAINING`, `STRESS_FAIL_RSS_BYTES`.

## Multi-profile scrape

Run this manual external-network scenario against an already-running server:

```bash
HOST=127.0.0.1:9222 HYPERION_AUTH_TOKEN=devtoken \
  PROFILE_COUNT=20 ./scripts/stress/run-profiles.sh
```

It creates one ephemeral browser context and target per profile, releases every client through a common start barrier, and uses `Promise.allSettled` so all profile clients issue their scrape concurrently. Hyperion's current browser task still executes commands serially, so p50/p95/p99 are queue-inclusive end-to-end latencies rather than a claim of simultaneous engine execution.

Defaults are `PROFILE_COUNT=20`, `SCRAPE_URL=https://example.com`, and expected title/H1 `Example Domain`. Any positive profile count is accepted, including values above 20. Override validation with `SCRAPE_EXPECT_TITLE` and `SCRAPE_EXPECT_H1`; setting either expectation to an empty string disables that assertion. Operational controls are `PROFILE_COMMAND_TIMEOUT_MS`, `PROFILE_FAIL_P99_MS`, `PROFILE_FAIL_RSS_BYTES` (0 disables the RSS limit), and `PROFILE_MAX_FAILURES`.

The run captures health, Prometheus gauges, RSS, per-profile content and latency, and writes `summary.json` plus `summary.txt` under gitignored `artifacts/stress-profiles-*`. A `finally` cleanup closes every target and disposes every created context. The command fails on excessive scrape failures, p99/RSS threshold violations, cleanup errors, or targets/contexts left beyond the pre-run baseline. Artifacts contain host, URL, generated context/target IDs, metrics, and results; the authentication token is never serialized.

## CI

[`.github/workflows/stress.yml`](../.github/workflows/stress.yml) is **manual or scheduled**, not on every pull request.

- `workflow_dispatch` mode `smoke` — short run + `selftest` against `HYPERION_MAX_TARGETS=4`
- `workflow_dispatch` mode `soak` or weekly cron — 2,000 targets, 300s hold, uploads artifacts

Do not treat a green soak on GitHub-hosted runners as a production capacity rating.

## Pass criteria (on the machine under test)

- RSS and the configured V8 heap budget stay within the thresholds you set
- p95/p99 client command/navigation latency stay within `STRESS_FAIL_P99_MS`
- overload returns `Overloaded:` rather than unbounded queues
- disconnect/close leaves `/json/list` empty (page targets)

Measure one worker, one node, and multiple replicas separately. Sticky-route CDP WebSockets per open profile across replicas.
