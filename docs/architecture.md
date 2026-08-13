# Architecture

Hyperion is a single-process scraping browser: Glommio (io_uring) for sockets and the CDP HTTP/WebSocket surface, V8 on a dedicated isolate pool, and a serialized “browser task” that owns all `Page` values (`!Send`).

```
                    ┌──────────────────────────────┐
  CDP clients  ───► │  accept loop (Glommio)       │
  dashboard/UI      │  per-connection HTTP/WS      │
                    └────────────┬─────────────────┘
                                 │ local_channel (bounded by HYPERION_MAX_QUEUED_COMMANDS)
                                 ▼
                    ┌──────────────────────────────┐
                    │  browser task                │
                    │  targets, sessions, previews │
                    │  profile manager, Fetch      │
                    └──────┬───────────┬───────────┘
                           │           │
                           ▼           ▼
                    BrowserContext   V8 pool (N OS threads,
                    HTTP client      many isolates)
                    CookieJar
```

## Crates

| Crate | Role |
|-------|------|
| `hyperion-cli` | `serve` / `smoke` / `fetch` |
| `hyperion-config` | `HYPERION_*` parsing, fail-closed bind, constant-time token compare |
| `hyperion-cdp` | HTTP router, RFC6455, command dispatch, live preview |
| `hyperion-browser` | Navigation, `Page`, DOM↔JS bridge |
| `hyperion-js` | V8 isolate pool, eval, remote objects |
| `hyperion-dom` | HTML parse tree |
| `hyperion-net` | HTTP client, cookies, SSRF, Fetch interception |
| `hyperion-profile` | Encrypted snapshots, local/S3 store, catalog leases |
| `hyperion-metrics` | Prometheus text + process gauges |
| `hyperion-dashboard` | Session/CSRF + embedded SPA (`include_str!` of `frontend/dist`) |

## Workers

`HYPERION_WORKERS` is the intended executor count. The current `serve` path starts **one** Glommio executor on the process bind address. Multi-replica deployments run multiple containers and sticky-route CDP sessions for an open profile to the owning node.

## Admission

The browser task serializes commands. Connection tasks refuse new work when `hyperion_queue_depth` reaches `HYPERION_MAX_QUEUED_COMMANDS`. Target, session, preview, navigation, and eval counts have separate caps. Rejections are CDP `-32000` with `Overloaded:` in the message (HTTP `/json/new` still returns **200** with that JSON error object).

## Profiles

Persistent profiles encrypt snapshots (ChaCha20-Poly1305) to local disk or S3. PostgreSQL, when configured, stores metadata, leases, and node heartbeats. Heartbeats run on the browser-task idle sweep (~5s). Ephemeral profiles stay in memory and are not catalogued.

## Dashboard

`/ui/` is served from the same bind as CDP. Production assets are built with Vite (`base: /ui/`) into unhashed `app.js` / `app.css` so the Rust crate can embed them. Hostile page HTML is never inlined into the operator DOM; the profile browser renders snapshots in a scriptless `srcdoc` iframe and talks to `/preview/live/<id>`.
