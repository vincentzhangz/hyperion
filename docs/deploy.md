# Deploy

## GHCR image

Images publish from [`.github/workflows/release.yml`](../.github/workflows/release.yml), not from pull-request CI.

| Ref | Tags |
|-----|------|
| `main` | `main`, `sha-<full git sha>` |
| `vX.Y.Z` | `X.Y.Z`, `X.Y`, `latest` |
| `workflow_dispatch` | same rules as the ref that was dispatched |

Image name: `ghcr.io/<github.repository_owner>/hyperion`.

The build pushes SBOM + SLSA provenance attestations and **keyless Cosign** signatures (`id-token: write`). Verify:

```bash
cosign verify \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'https://github.com/vincentzhangz/hyperion/.github/workflows/release.yml@.*' \
  ghcr.io/vincentzhangz/hyperion@sha256:<digest>
```

Pull a specific immutable tag in production (`sha-…` or `vX.Y.Z`), not only `latest`. Do not `docker push` images by hand; the release workflow is the only publisher.

## Environment

See [configuration.md](configuration.md) for the full table. Production must set `HYPERION_AUTH_TOKEN` and, for persistent profiles, `HYPERION_ENCRYPTION_KEY`. Do not ship compose files with `changeme` secrets.

## Reverse proxy

Terminate TLS in nginx/Caddy/Envoy. Forward `/`, `/json`, `/devtools`, `/preview`, `/ui`, `/api`, `/metrics`, `/health`. Sticky-route CDP WebSockets for an **open profile** to the owning replica.

## io_uring

- Linux 5.8+
- seccomp that allows `io_uring_*` (see `seccomp.json`)
- `memlock` ≥ 8 MiB
- `CAP_SYS_RESOURCE` (or equivalent) to raise the rlimit in containers

## Production image locally

```bash
docker build -f Dockerfile.prod -t hyperion:release .
```

The image rebuilds dashboard assets, embeds them, runs as uid 10001, and healthchecks `GET /health/live` with curl.

Run as non-root, read-only root filesystem, tmpfs for `/tmp`, mounted profile volume, and a secrets file for the token and encryption key. Mount `seccomp.json` if the orchestrator does not already apply it.

## Compose example

`docker-compose.prod.yml` pulls `${GHCR_IMAGE:-ghcr.io/vincentzhangz/hyperion:latest}` and **requires** `.env` values for `HYPERION_AUTH_TOKEN`, `HYPERION_ENCRYPTION_KEY`, `POSTGRES_PASSWORD`, and MinIO credentials. Copy `.env.example` and fill them in.

```bash
cp .env.example .env
# edit secrets
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
curl -fsS http://127.0.0.1:9222/health/live
```

## Backup / restore

- **Local profiles:** tar `HYPERION_PROFILE_ROOT` after a drain (`GET /health/ready` → 503, `hyperion_sessions 0`). Snapshots are already encrypted.
- **S3:** versioned bucket + standard object-store backup. Catalog metadata is in PostgreSQL (`hyperion_profiles`, leases, heartbeats).
- **Restore:** restore objects first, then Postgres, then start a replica with the same `HYPERION_ENCRYPTION_KEY`. Stale leases expire (~60s) via heartbeat fencing.

## Alerts

Scrape `GET /metrics` (auth required when a token is configured). Useful series:

- `hyperion_admission_rejected_total` — saturation
- `hyperion_queue_depth` / `hyperion_queue_drops_total`
- `hyperion_targets_open`, `hyperion_sessions`, `hyperion_previews`
- `hyperion_rss_bytes`, `hyperion_v8_heap_bytes` (configured budget × live isolates), `hyperion_cache_bytes`
- `hyperion_fetch_timeouts_total`, `hyperion_lease_contention_total`
- `hyperion_shutdown` — replica is draining
