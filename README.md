# Hyperion

[![CI](https://github.com/vincentzhangz/hyperion/actions/workflows/ci.yml/badge.svg)](https://github.com/vincentzhangz/hyperion/actions/workflows/ci.yml)
[![GHCR](https://img.shields.io/badge/ghcr-hyperion-2496ED)](https://github.com/vincentzhangz/hyperion/pkgs/container/hyperion)
[![Release](https://img.shields.io/github/v/release/vincentzhangz/hyperion)](https://github.com/vincentzhangz/hyperion/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.97-orange.svg)](rust-toolchain.toml)

Headless browser on **Glommio 0.9** (io_uring) and **V8**, with a Puppeteer/Playwright-drivable **CDP subset**, isolated **browser profiles**, and a **cluster operations dashboard**.

Hyperion is a single-tenant Linux scraping service: trusted operators, untrusted URLs. It does **not** render pixels, screenshots, or PDFs, and it is **not** a Chrome replacement.

## Quick start (GHCR)

Pull the tagged production image, supply secrets via the environment (never commit them), mount the profile volume and seccomp policy, then verify liveness.

```bash
cp .env.example .env
# Set HYPERION_AUTH_TOKEN (and HYPERION_ENCRYPTION_KEY for persistent profiles).

export GHCR_IMAGE=ghcr.io/vincentzhangz/hyperion:latest   # or a vX.Y.Z / sha-… tag
docker pull "$GHCR_IMAGE"

docker run --rm \
  --name hyperion \
  --env-file .env \
  -e HYPERION_BIND=0.0.0.0:9222 \
  -p 9222:9222 \
  --security-opt seccomp=./seccomp.json \
  --cap-add SYS_RESOURCE \
  --ulimit memlock=8388608:8388608 \
  --read-only \
  --tmpfs /tmp \
  -v hyperion-profiles:/var/lib/hyperion/profiles \
  "$GHCR_IMAGE"

curl -fsS http://127.0.0.1:9222/health/live
# ok
```

Verify a release tag and its Cosign signature (keyless, GitHub Actions identity):

```bash
cosign verify \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'https://github.com/vincentzhangz/hyperion/.github/workflows/release.yml@.*' \
  ghcr.io/vincentzhangz/hyperion:latest
```

A Postgres + MinIO example is in [`docker-compose.prod.yml`](docker-compose.prod.yml). See [production deployment](docs/deploy.md).

## Local development

Hyperion needs **Linux + io_uring** (kernel 5.8+). On macOS, use Docker with the checked-in seccomp profile.

Prerequisites: Docker (or a Linux host with Rust 1.97, CMake, Python 3, pkg-config, libssl, glib), Node.js 20 for the dashboard.

```bash
git clone https://github.com/vincentzhangz/hyperion.git
cd hyperion

docker compose build app

# Format, lint, and serialized workspace tests (V8 is not safe to parallelize).
docker compose run --rm app cargo fmt --all -- --check
docker compose run --rm app cargo clippy --workspace --all-targets -- -D warnings
docker compose run --rm app cargo test --workspace -j 1 -- --test-threads=1

# Dev server + dashboard (token required on non-loopback binds).
docker compose run --rm -p 9222:9222 \
  -e HYPERION_AUTH_TOKEN=devtoken \
  -e HYPERION_ALLOW_PRIVATE_NETWORK=true \
  app cargo run -p hyperion-cli -- serve 0.0.0.0:9222 --preview
```

Dashboard frontend (source of truth is `crates/hyperion-dashboard/frontend/src`; `dist/` is generated and committed because Rust embeds it at compile time):

```bash
cd crates/hyperion-dashboard/frontend
npm ci
npm test
npm run build          # writes dist/{index.html,app.js,app.css}
npx playwright test    # set HYPERION_E2E=1 against a live server for login/preview
```

## Stress testing

Do not run the multi-hour 2,000-target soak on every pull request. Results are hardware-specific; treat them as a gate on the machine that produced them, not as a portable SLA.

```bash
# Short smoke against a running server (also used by .github/workflows/stress.yml).
HOST=127.0.0.1:9222 HYPERION_AUTH_TOKEN=devtoken ./scripts/stress/run.sh smoke

# Low-limit harness check: start Hyperion with HYPERION_MAX_TARGETS=2, then:
./scripts/stress/run.sh selftest

# Configurable local run
STRESS_TARGETS=64 STRESS_HOLD_SECS=30 STRESS_FAIL_P99_MS=8000 \
  ./scripts/stress/run.sh smoke

# Production-shaped soak (2,000 targets). Scheduled/manual via stress.yml; hold
# time defaults to 300s in CI. Raise STRESS_HOLD_SECS locally for multi-hour soaks.
STRESS_TARGETS=2000 STRESS_HOLD_SECS=7200 ./scripts/stress/run.sh soak

# Manual external-network scenario: 20 isolated ephemeral profiles scrape
# example.com. PROFILE_COUNT accepts any positive value, including 20+.
PROFILE_COUNT=20 ./scripts/stress/run-profiles.sh
```

Thresholds: `STRESS_FAIL_P99_MS`, `STRESS_FAIL_CLEANUP_REMAINING` (default 0), `STRESS_FAIL_RSS_BYTES` (0 = disabled). Artifacts land in gitignored `artifacts/stress-*` (`summary.json` + `summary.txt`). Methodology: [docs/benchmarks.md](docs/benchmarks.md).

The profile scenario is not part of routine CI because it needs external network access. Its clients start together, while Hyperion's current browser task executes their commands serially; reported profile latency therefore includes queueing. It writes `artifacts/stress-profiles-*` and fails on scrape errors, thresholds, cleanup errors, or leaked targets/contexts.

## Supported scope

- CDP subset for scraping: Target, Page.navigate, Runtime.evaluate + remote objects, DOM queries, Network cookies/headers, Fetch interception, Input via JS dispatch, Hyperion.Profile.*
- Isolated persistent/ephemeral profiles (encrypted local or S3 payloads, optional Postgres catalog)
- Operations dashboard at `/ui/` (session cookie + CSRF)
- Prometheus metrics, `/health/live` (unauthenticated), `/health/ready`

## Explicit limitations

- No `Page.captureScreenshot`, screencast, PDF, or pixel framebuffer
- No real layout engine (box model / hit testing are approximate)
- Not full Chrome: no service workers, IndexedDB, extensions, or accessibility/CSS/Debugger/Tracing domains
- `HYPERION_DASHBOARD_BIND`, `HYPERION_PREVIEW_ORIGIN`, and `HYPERION_PROMETHEUS_URL` are parsed but unused (dashboard shares the CDP bind; preview is same-origin; metrics are scrape-only)

See [docs/cdp-matrix.md](docs/cdp-matrix.md).

## Security posture

Trusted operators, untrusted page URLs. Non-loopback binds fail closed without `HYPERION_AUTH_TOKEN`. SSRF denies private/metadata destinations unless explicitly overridden. Previews are sandboxed iframes. Report vulnerabilities per [SECURITY.md](SECURITY.md). Details: [docs/security.md](docs/security.md).

## Architecture

One Glommio executor per worker (io_uring), a shared V8 isolate pool, a serialized CDP browser task, and profile isolation via separate HTTP clients and encrypted snapshots. See [docs/architecture.md](docs/architecture.md).

## Configuration

All settings are `HYPERION_*` environment variables. See [docs/configuration.md](docs/configuration.md).

## Production deployment

Use the GHCR image, seccomp + memlock, a reverse proxy for TLS, and sticky routing for open profiles. [docs/deploy.md](docs/deploy.md) · [docs/runbook.md](docs/runbook.md).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) · [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Apache-2.0 ([LICENSE](LICENSE)).

## Documentation

Operator and development guides live in [`docs/`](docs/).
