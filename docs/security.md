# Security

Hyperion is operated by trusted staff against **untrusted page URLs**.

## Authentication

- Every control-plane route except `GET /health/live` and the dashboard static assets (`/ui/`, `/ui/app.js`, `/ui/app.css`) requires a valid bearer token when `HYPERION_AUTH_TOKEN` is set, and **always** when the bind address is not loopback.
- Comparison is constant-time (`subtle`).
- The dashboard exchanges the bearer token for an `HttpOnly; SameSite=Strict` session cookie. `Secure` is set only when the login request carries `X-Forwarded-Proto: https` (TLS at the reverse proxy). Tokens are never placed in URLs or `localStorage`.
- Destructive dashboard actions require the `X-CSRF-Token` header (403 otherwise). Sessions expire (`HYPERION_SESSION_TTL_SECS`).
- Bidirectional live-preview WebSockets accept JSON commands only after a CSRF `hello` when the upgrade used a session cookie (bearer-authenticated CDP clients are unchanged). `Origin` must match the request `Host`. Preview URLs never carry bearer tokens.

## Bind fail-closed

Binding `0.0.0.0` (or any non-loopback address) without `HYPERION_AUTH_TOKEN` is a startup error.

TLS terminates at the reverse proxy. Hyperion speaks plaintext HTTP/WS on the private bind.

## SSRF

Outbound fetches resolve DNS and reject:

- loopback, private, link-local, multicast, unspecified
- `169.254.169.254` and other metadata hosts
- redirects to those destinations

Override only with `HYPERION_ALLOW_PRIVATE_NETWORK=true` for trusted lab networks.

## Isolation

Profiles never share cookies, `localStorage`, HTTP clients, or UA/header overrides. `sessionStorage` is page-scoped. IndexedDB, service workers, password stores, and extensions are **unsupported** (not silently faked).

Persistent snapshots are ChaCha20-Poly1305 encrypted. Keys come from `HYPERION_ENCRYPTION_KEY`. Profile contents, cookies, and keys are never logged.

## Preview HTML

Live preview and the dashboard profile browser render untrusted HTML in a sandboxed iframe (`sandbox="allow-same-origin"` without scripts, forms, popups, or top-navigation). Do not insert page HTML into the dashboard DOM. Operator clicks are addressed by stable `data-hyperion-node-id` values and executed in the headless DOM, not by running mirrored scripts in the operator origin.

## Supply chain

Release images are signed keyless with Cosign and published with SBOM/provenance attestations. Verify signatures before promoting a tag. See [deploy.md](deploy.md).

## Reporting

See [SECURITY.md](../SECURITY.md).
