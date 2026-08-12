# Development

## Platform

Linux 5.8+ with io_uring. macOS and Windows hosts should use `docker compose` and `seccomp.json` (Docker’s default seccomp returns `EPERM` for `io_uring_*`).

The container also needs `CAP_SYS_RESOURCE` and `memlock` ≥ 8 MiB.

## Commands

```bash
docker compose build app
docker compose run --rm app cargo fmt --all
docker compose run --rm app cargo fmt --all -- --check
docker compose run --rm app cargo clippy --workspace --all-targets -- -D warnings
docker compose run --rm app cargo test --workspace -j 1 -- --test-threads=1
docker compose run --rm app cargo build --release -p hyperion-cli
```

Always serialize V8-heavy tests (`-j 1` and `--test-threads=1`).

## Dashboard

```bash
cd crates/hyperion-dashboard/frontend
npm ci
npm test                 # vitest
npm run build            # tsc --noEmit && vite build
npx playwright test      # optional; HYPERION_E2E=1 against a live server
```

`crates/hyperion-dashboard/src/lib.rs` embeds `frontend/dist/{index.html,app.js,app.css}`. Vite is configured with `base: "/ui/"` and **unhashed** filenames. After UI changes, commit a matching `dist/` — CI fails `git diff --exit-code` on that directory.

## Production image (local)

```bash
docker build -f Dockerfile.prod -t hyperion:local .
```

## Stress harness

See the README “Stress testing” section and [benchmarks.md](benchmarks.md). The harness lives in `scripts/stress/` and writes `artifacts/stress-*` (gitignored).
