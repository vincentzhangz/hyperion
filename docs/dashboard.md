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
| GET | `/api/v1/dashboard/overview` | Metrics snapshot, local pages (`profileId`), `stale` if the catalog node list fails |
| GET | `/api/v1/dashboard/profiles?q=&offset=0&limit=100` | Authoritative **catalog** list (persistent) plus loaded ephemeral. Each row includes `local` (this node holds the lease). |
| POST | `/api/v1/dashboard/profiles?action=create\|flush\|delete\|open&id=&name=` | CSRF required. Create is persistent. `open` loads a local profile. |
| GET | `/api/v1/dashboard/profiles/detail?id=` | Local profile metadata plus page targets for that `profileId`. Remote-owned profiles return `not_local`. |
| POST | `/api/v1/dashboard/targets?action=create\|navigate\|close\|activate&profileId=&id=&url=` | CSRF required. Create/navigate run SSRF checks. Ownership mismatches return `target_profile_mismatch`. |
| GET | `/api/v1/dashboard/nodes` | Local worker row + catalog heartbeat node ids; `stale` on catalog error |
| GET | `/api/v1/dashboard/events` | Bounded in-memory audit log (no secrets) |

Pagination is `offset` / `limit` on the profiles collection. There is no multi-replica live aggregation: other nodes appear only as catalog heartbeat ids. When the catalog is unreachable, `stale: true` is set and the UI shows a warning.

## Lifecycle actions

Create / flush / delete go through the same profile manager as `Hyperion.Profile.*`. Delete tombstones the catalog row and drops the in-memory context. Flush is a no-op for ephemeral profiles.

## Previews

The dashboard **profile browser** is an interactive **DOM/HTML mirror**, not a pixel-accurate Chrome view. Open **Open browser** on a locally owned profile (or **Open** on an overview page that belongs to one). The UI connects to `/preview/live/<targetId>` from React, renders snapshots into a `srcdoc` iframe with `sandbox="allow-same-origin"` (no scripts, forms, popups, or top-navigation), and forwards node-addressed click/hover/focus/keyboard/scroll using `data-hyperion-node-id`.

Interactive preview commands require the session cookie, a matching WebSocket `Origin`, and a CSRF `hello` before input. The first mutating preview message proves `X-CSRF` via that hello payload. Tokens are never placed in the preview URL. Remote-owned profiles are shown as unavailable; commands are not sent to another node.

Standalone `/preview/<id>` remains a read-only viewer for CDP operators. Closing a target detaches preview subscribers. Catalog `owner_node` is displayed on profile rows so operators can see which node holds the lease.

Fidelity limits: no layout engine, no hit-testing from coordinates, no page JS in the operator iframe, and no Chrome-compatible painting. Link navigation and URL-bar changes go through Hyperion (SSRF + profile admission), not the operator iframe.

## Rebuild

```bash
cd crates/hyperion-dashboard/frontend && npm ci && npm test && npm run build
```
