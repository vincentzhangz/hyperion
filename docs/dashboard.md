# Dashboard

The operations UI is served at `/ui/` from the Hyperion process (no Node.js at runtime). Source lives in `crates/hyperion-dashboard/frontend/src` (React + Vite + Tailwind). Production assets are the committed `frontend/dist/{index.html,app.js,app.css}` files embedded by `include_str!`.

CI runs `npm ci`, vitest, `npm run build`, a clean-tree check that `dist/` matches source, and Playwright against a live server.

## Auth

- `POST /api/v1/dashboard/login` with `Authorization: Bearer <token>` sets `hyperion_session` (`HttpOnly; SameSite=Strict`; `Secure` only behind `X-Forwarded-Proto: https`) and returns `{ "csrf": "..." }`.
- Sessions expire after `HYPERION_SESSION_TTL_SECS` (default 8h). Expired cookies fail subsequent API calls with 401.
- Mutating requests (`POST`) require `X-CSRF-Token` matching the session. Missing/wrong CSRF → **403**.
- `/ui/` and `/ui/app.js` / `/ui/app.css` are reachable without a session so the login page can load. `/health/live` is unauthenticated. Other APIs follow the process token policy.

## APIs

| Method | Path | Notes |
|--------|------|-------|
| GET | `/api/v1/dashboard/overview` | Metrics snapshot, local pages, `stale` if the catalog node list fails |
| GET | `/api/v1/dashboard/profiles?q=&offset=0&limit=100` | Authoritative **catalog** list (persistent) plus loaded ephemeral |
| POST | `/api/v1/dashboard/profiles?action=create\|flush\|delete&id=&name=` | CSRF required. Create is persistent. |
| GET | `/api/v1/dashboard/nodes` | Local worker row + catalog heartbeat node ids; `stale` on catalog error |
| GET | `/api/v1/dashboard/events` | Bounded in-memory audit log (no secrets) |

Pagination is `offset` / `limit` on the profiles collection. There is no multi-replica live aggregation: other nodes appear only as catalog heartbeat ids. When the catalog is unreachable, `stale: true` is set and the UI shows a warning.

## Lifecycle actions

Create / flush / delete go through the same profile manager as `Hyperion.Profile.*`. Delete tombstones the catalog row and drops the in-memory context. Flush is a no-op for ephemeral profiles.

## Previews

The UI opens `/preview/<id>` in an iframe with `sandbox=""` (no scripts, forms, or top-navigation). Only **local** targets can be previewed; there is no proxy to another replica’s page. Closing a target detaches preview subscribers and clears session maps (stream cleanup). Ownership is the local process: catalog `owner_node` is displayed on profile rows so operators can see which node holds the lease.

## Rebuild

```bash
cd crates/hyperion-dashboard/frontend && npm ci && npm test && npm run build
```
