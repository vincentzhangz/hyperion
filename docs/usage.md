# Hyperion usage

## Serve CDP + live preview + dashboard

```bash
# Inside Docker (recommended on macOS hosts):
export HYPERION_AUTH_TOKEN=devtoken
docker compose run --rm -p 9222:9222 -e HYPERION_AUTH_TOKEN \
  app cargo run -p hyperion-cli -- serve 0.0.0.0:9222 --preview
```

Printed endpoints:

- DevTools: `ws://<host>:9222/devtools/browser/...` (also listed at `/json/version`)
- Live preview: `http://<host>:9222/preview`
- Dashboard: `http://<host>:9222/ui/`
- Liveness: `http://<host>:9222/health/live` (no auth)
- Metrics: `http://<host>:9222/metrics`

Non-loopback binds require `Authorization: Bearer <token>` on every control-plane route except liveness.

Open `/preview`, pick a page target, or create one:

```bash
curl -H "Authorization: Bearer $HYPERION_AUTH_TOKEN" \
  "http://127.0.0.1:9222/json/new?url=https://example.com"
```

The preview is a **DOM/HTML mirror** (`iframe` + `srcdoc`), not a pixel framebuffer. Relative URLs resolve via an injected `<base href>`. The dashboard **Open browser** workspace can click, type, and scroll against that mirror using stable node IDs; it is not Chrome-accurate rendering.

## Drive with CDP (Puppeteer / Playwright style)

Connect to a page WebSocket from `/json/list` → `webSocketDebuggerUrl`, then:

1. `Runtime.enable` / `Page.enable` / `Network.enable` / `DOM.enable` as needed
2. `Page.navigate` `{ "url": "..." }`
3. `Runtime.evaluate` `{ "expression": "document.title", "returnByValue": true }` — DOM APIs work via the in-V8 bridge

### Cookies

`Network.setCookie` / `getCookies` / `getAllCookies` / `deleteCookies` / `clearBrowserCookies` talk to the shared `CookieJar` on the HTTP client.

### Request interception

```text
Fetch.enable → Page.navigate → Fetch.requestPaused
  → Fetch.continueRequest | Fetch.fulfillRequest | Fetch.failRequest
```

Decisions are applied on the connection side-channel so a paused navigation can unblock without deadlocking the browser task.

### Limitations

- No screenshots / screencast / PDF
- Mouse/box-model by coordinates are best-effort (no layout engine)
- External `<script src>` / CSS loading is optional / incomplete vs a full browser

## Library smoke path

```rust
use hyperion_browser::BrowserContext;
use hyperion_net::client::HttpClientOptions;

// On a Glommio executor:
let ctx = BrowserContext::new(HttpClientOptions::default());
let page = ctx.navigate("https://example.com").await?;
assert!(!page.title().is_empty());
let _ = page.evaluate_js("document.querySelector('h1')?.textContent");
```
