# Profiles

Persistent named profiles and ephemeral incognito contexts.

## CDP

- `Hyperion.Profile.create/open/close/delete/flush/list`
- `Target.createBrowserContext` → ephemeral profile (incognito)
- `Target.disposeBrowserContext` closes that profile and its pages

`list` returns **catalog** rows (persistent profiles, including `owner_node`) plus loaded ephemeral profiles. Profile **names** are metadata. Internal ids are random hex (no path traversal).

## What is stored

Cookies, `localStorage`, UA/headers/locale/timezone/viewport/proxy settings.

**Not stored:** open pages, `sessionStorage`, remote objects, pending Fetch, sockets.

## Backends

- **Local:** `HYPERION_PROFILE_ROOT`, `0700` dirs, temp + fsync + rename.
- **S3:** SigV4 PUT/GET/DELETE, local read-through cache.
- **Catalog:** PostgreSQL for metadata/leases/heartbeats/tombstones. Payloads stay in the object store. If `HYPERION_POSTGRES_URL` is unset or connect fails, Hyperion uses an in-memory catalog (single process only).

## Leases and heartbeats

A persistent profile has a single writer. `Hyperion.Profile.open` fails with lease held if another node owns it (`hyperion_lease_contention_total`). The serving process heartbeats its `HYPERION_NODE_ID` on the browser-task idle sweep (~5s). Postgres leases last 60 seconds and can be taken over after expiry.

## Encryption

ChaCha20-Poly1305 envelope (nonce || ciphertext). Checksummed JSON snapshots (`version` + sha256). Unsupported future versions are rejected. Set `HYPERION_ENCRYPTION_KEY` in production so restarts can decrypt existing snapshots.

## Backup

See [deploy.md](deploy.md). Dashboard listings use the catalog, not only in-memory `list_loaded()`.
