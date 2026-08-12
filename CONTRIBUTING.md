# Contributing to Hyperion

## Development environment

Hyperion requires Linux io_uring (kernel 5.8+). On macOS, use the Docker workflow in the README.

```bash
docker compose build app
docker compose run --rm app cargo fmt --all -- --check
docker compose run --rm app cargo clippy --workspace --all-targets -- -D warnings
docker compose run --rm app cargo test --workspace -j 1 -- --test-threads=1
```

Dashboard UI:

```bash
cd crates/hyperion-dashboard/frontend
npm ci
npm run build
```

Commit the generated `frontend/dist/{index.html,app.js,app.css}` with source changes. CI fails if dist is stale.

## Pull requests

- Keep diffs focused. Do not mix refactors with feature work.
- Add or update tests for behavior changes.
- Update docs when configuration, APIs, or operator workflows change.
- Do not commit secrets, core dumps, or `target/` artifacts.
- Run fmt, clippy, and tests before opening a PR.

## Scope

Hyperion is a scraping CDP subset. Do not add screenshot/PDF/layout claims or treat it as a Chrome replacement.
