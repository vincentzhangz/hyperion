# Operations runbook

## Health

- `GET /health/live` — process is up (no auth).
- `GET /health/ready` — not draining (auth required when token configured).
- `GET /metrics` — Prometheus text (auth required when token configured).

Readiness goes false when Hyperion begins shutdown (`hyperion_shutdown 1`).

## SIGTERM

The CLI sets a shutdown flag on SIGINT/SIGTERM. The accept loop stops, sessions are closed, dirty profiles are flushed, pending Fetch waits time out, and the process exits within `HYPERION_SHUTDOWN_GRACE_SECS`.

## Overload

Admission rejects with CDP error `-32000 Overloaded: ...` when targets, sessions, previews, queued commands, active navigations, or active evals exceed configured limits. HTTP `/json/new` returns **status 200** with that JSON error object. Clients should back off. Saturation is visible as `hyperion_admission_rejected_total`.

Closing a target drops its sessions and preview subscribers and updates `hyperion_targets_open` / `hyperion_sessions` / `hyperion_previews`.

## Profile leases

A persistent profile has a single writer. `Hyperion.Profile.open` fails with lease held if another node owns it. PostgreSQL heartbeats (browser-task sweep) fence expired nodes.

## Common failures

| Symptom | Check |
|---------|-------|
| Bind error on 0.0.0.0 | Token missing |
| `blocked destination` | SSRF policy; set allow-private only if intended |
| Fetch hangs | Client must answer `Fetch.requestPaused` within the pause timeout |
| V8 eval deadline | `HYPERION_EVAL_DEADLINE_SECS` |
| io_uring EPERM | seccomp profile / memlock |
| Dashboard 403 on POST | Missing `X-CSRF-Token` |
| Dashboard 401 | Session expired or cookie missing |
| `stale` in dashboard | Catalog (Postgres) unreachable; local worker data only |
| GHCR pull denied | Package permissions / `docker login ghcr.io` |

## Drain / deploy

1. Stop sending new sticky sessions to the replica.
2. SIGTERM.
3. Wait for ready=503 and zero `hyperion_sessions`.
4. Replace the container with a pinned GHCR digest (`sha-…` or `vX.Y.Z`).
5. Verify `/health/live` and Cosign as in [deploy.md](deploy.md).

## Alerts

See the metrics list in [deploy.md](deploy.md). Page on a rising `hyperion_admission_rejected_total` plus non-zero `hyperion_queue_depth`, and on `hyperion_rss_bytes` crossing the host budget you measured in soak tests (do not copy a number from another machine).
