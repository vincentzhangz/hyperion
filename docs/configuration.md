# Configuration

All settings are environment variables. Secrets are never printed via `Debug`. Non-loopback binds **fail closed** without `HYPERION_AUTH_TOKEN`.

| Variable | Default | Notes |
|----------|---------|-------|
| `HYPERION_BIND` | `127.0.0.1:9222` | Socket address |
| `HYPERION_NODE_ID` | hostname or random | Catalog / lease identity |
| `HYPERION_AUTH_TOKEN` | unset | Bearer token; required off-loopback |
| `HYPERION_ALLOW_PRIVATE_NETWORK` | `false` | SSRF override for lab networks |
| `HYPERION_MAX_HEADER_BYTES` | 64 KiB | HTTP request head |
| `HYPERION_MAX_WS_MESSAGE_BYTES` | 8 MiB | WebSocket frame |
| `HYPERION_MAX_BODY_BYTES` | 16 MiB | Response body |
| `HYPERION_FETCH_TIMEOUT_SECS` | `30` | Outbound fetch |
| `HYPERION_FETCH_PAUSE_TIMEOUT_SECS` | `30` | Abandoned `Fetch.requestPaused` |
| `HYPERION_SHUTDOWN_GRACE_SECS` | `15` | Drain window |
| `HYPERION_MAX_TARGETS` | `2048` | Page targets (browser target excluded from the cap) |
| `HYPERION_MAX_SESSIONS` | `4096` | CDP sessions |
| `HYPERION_MAX_PREVIEWS` | `256` | Live-preview subscribers |
| `HYPERION_MAX_REMOTE_OBJECTS` | `10000` | Per-isolate object registry |
| `HYPERION_MAX_RESPONSE_CACHE_BYTES` | 64 MiB | `Network.getResponseBody` cache |
| `HYPERION_MAX_ACTIVE_NAVIGATIONS` | `128` | In-flight navigations |
| `HYPERION_MAX_ACTIVE_EVALS` | `64` | In-flight `Runtime.evaluate` |
| `HYPERION_MAX_QUEUED_COMMANDS` | `1024` | Browser-task queue |
| `HYPERION_PER_ORIGIN_CONNECTIONS` | `8` | HTTP pool per origin |
| `HYPERION_IDLE_TARGET_TTL_SECS` | `1800` | Idle page sweep |
| `HYPERION_WORKERS` | `1` | Documented replica sizing; current process uses one executor |
| `HYPERION_V8_POOL_SIZE` | `4` | V8 OS threads |
| `HYPERION_V8_HEAP_MB` | `128` | Per-isolate heap budget (metrics estimate = live isolates × this) |
| `HYPERION_EVAL_DEADLINE_SECS` | `10` | V8 eval deadline |
| `HYPERION_LOG_LEVEL` | `info` | tracing filter |
| `HYPERION_LOG_FORMAT` | `json` | `json` or text |
| `HYPERION_PROFILE_ROOT` | `/var/lib/hyperion/profiles` | Local store |
| `HYPERION_PROFILE_BACKEND` | `local` | `local` or `s3` |
| `HYPERION_S3_ENDPOINT` / `BUCKET` / `REGION` / `ACCESS_KEY` / `SECRET_KEY` | unset | S3 store |
| `HYPERION_POSTGRES_URL` | unset | Catalog; memory catalog if unset or connect fails |
| `HYPERION_ENCRYPTION_KEY` | random if unset | 32 bytes or 64 hex chars |
| `HYPERION_ENCRYPTION_KEY_FILE` | unset | File containing the key; used if the env var is unset |
| `HYPERION_SESSION_TTL_SECS` | `28800` | Dashboard cookie |
| `HYPERION_DASHBOARD_BIND` | unset | **Parsed, unused** — dashboard shares `HYPERION_BIND` |
| `HYPERION_PREVIEW_ORIGIN` | unset | **Parsed, unused** — preview is same-origin `/preview/<id>` |
| `HYPERION_PROMETHEUS_URL` | unset | **Parsed, unused** — scrape `GET /metrics`; no push gateway |

CLI `hyperion serve <bind> --preview` overrides the bind address.
